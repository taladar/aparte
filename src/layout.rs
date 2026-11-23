/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CONSOLE_WINDOW: &str = "console";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub account: String,
    pub jid: String,
    /// Present for channels only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nick: Option<String>,
}

// TOML requires plain values before arrays of tables, so `active` must stay first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub windows: Vec<SavedWindow>,
}

impl Layout {
    fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content)
                .with_context(|| format!("Malformed layout file {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => {
                Err(err).with_context(|| format!("Cannot read layout file {}", path.display()))
            }
        }
    }

    // The terminal can be closed at any moment, so a partial write must never replace the file.
    fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string(self).context("Cannot serialize layout")?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, content).with_context(|| format!("Cannot write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("Cannot replace {}", path.display()))
    }
}

/// Keeps the on-disk layout in sync with the open windows.
///
/// Windows of an account stay in the saved layout until that account has
/// connected, so starting offline or with one account down loses nothing.
#[derive(Debug)]
pub struct LayoutTracker {
    path: PathBuf,
    saved: Layout,
    restored_accounts: HashSet<String>,
    pending_active: Option<String>,
}

impl LayoutTracker {
    pub fn load(path: PathBuf) -> Self {
        let saved = Layout::load(&path).unwrap_or_else(|err| {
            log::warn!("Ignoring saved layout: {err:#}");
            Layout::default()
        });
        Self {
            path,
            saved,
            restored_accounts: HashSet::new(),
            pending_active: None,
        }
    }

    /// Windows to reopen for `account`. Empty after its first connection, so
    /// that a reconnect does not bring back windows closed in the meantime.
    pub fn begin_restore(&mut self, account: &str) -> Vec<SavedWindow> {
        if !self.restored_accounts.insert(account.to_string()) {
            return Vec::new();
        }
        let windows: Vec<SavedWindow> = self
            .saved
            .windows
            .iter()
            .filter(|window| window.account == account)
            .cloned()
            .collect();
        if let Some(active) = &self.saved.active {
            if windows.iter().any(|window| &window.jid == active) {
                self.pending_active = Some(active.clone());
            }
        }
        windows
    }

    /// The saved active window once it is open, unless the user already moved on.
    pub fn take_pending_active(
        &mut self,
        windows: &[String],
        current: Option<&str>,
    ) -> Option<String> {
        let pending = self.pending_active.as_ref()?;
        if !is_console(current) {
            self.pending_active = None;
            return None;
        }
        if windows.contains(pending) {
            return self.pending_active.take();
        }
        None
    }

    pub fn update(&mut self, windows: Vec<SavedWindow>, active: Option<String>) {
        let merged = self.merged(windows, active);
        if merged == self.saved {
            return;
        }
        if let Err(err) = merged.save(&self.path) {
            log::error!("Cannot save layout: {err:#}");
        }
        self.saved = merged;
    }

    fn merged(&self, mut windows: Vec<SavedWindow>, active: Option<String>) -> Layout {
        for saved in &self.saved.windows {
            if !self.restored_accounts.contains(&saved.account)
                && !windows.iter().any(|window| window.jid == saved.jid)
            {
                windows.push(saved.clone());
            }
        }
        let active = if is_console(active.as_deref()) && self.is_awaiting_active() {
            self.saved.active.clone()
        } else {
            active
        };
        Layout { active, windows }
    }

    fn is_awaiting_active(&self) -> bool {
        self.pending_active.is_some()
            || self.saved.windows.iter().any(|window| {
                Some(&window.jid) == self.saved.active.as_ref()
                    && !self.restored_accounts.contains(&window.account)
            })
    }
}

fn is_console(window: Option<&str>) -> bool {
    window.is_none_or(|window| window == CONSOLE_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "alice@example.org";
    const BOB: &str = "bob@example.org";

    fn chat(account: &str, jid: &str) -> SavedWindow {
        SavedWindow {
            account: account.to_string(),
            jid: jid.to_string(),
            nick: None,
        }
    }

    fn tracker_with(dir: &tempfile::TempDir, layout: &Layout) -> LayoutTracker {
        let path = dir.path().join("layout.toml");
        layout.save(&path).unwrap();
        LayoutTracker::load(path)
    }

    fn on_disk(dir: &tempfile::TempDir) -> Layout {
        Layout::load(&dir.path().join("layout.toml")).unwrap()
    }

    #[test]
    fn layout_with_channel_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            active: Some("room@muc.example.org".to_string()),
            windows: vec![
                chat(ALICE, "contact@example.org"),
                SavedWindow {
                    account: ALICE.to_string(),
                    jid: "room@muc.example.org".to_string(),
                    nick: Some("alice".to_string()),
                },
            ],
        };
        tracker_with(&dir, &layout);
        assert_eq!(on_disk(&dir), layout);
    }

    #[test]
    fn malformed_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.toml");
        fs::write(&path, "not [valid").unwrap();
        let mut tracker = LayoutTracker::load(path);
        assert!(tracker.begin_restore(ALICE).is_empty());
    }

    #[test]
    fn startup_on_console_keeps_saved_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            active: Some("contact@example.org".to_string()),
            windows: vec![chat(ALICE, "contact@example.org")],
        };
        let mut tracker = tracker_with(&dir, &layout);
        tracker.update(Vec::new(), Some(CONSOLE_WINDOW.to_string()));
        assert_eq!(on_disk(&dir), layout);
    }

    #[test]
    fn reconnect_does_not_reopen_windows() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            active: None,
            windows: vec![chat(ALICE, "contact@example.org")],
        };
        let mut tracker = tracker_with(&dir, &layout);
        assert_eq!(tracker.begin_restore(ALICE).len(), 1);
        assert!(tracker.begin_restore(ALICE).is_empty());
    }

    #[test]
    fn closed_window_leaves_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            active: Some(CONSOLE_WINDOW.to_string()),
            windows: vec![chat(ALICE, "contact@example.org")],
        };
        let mut tracker = tracker_with(&dir, &layout);
        tracker.begin_restore(ALICE);
        tracker.update(Vec::new(), Some(CONSOLE_WINDOW.to_string()));
        assert!(on_disk(&dir).windows.is_empty());
    }

    #[test]
    fn windows_of_unconnected_account_survive() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            active: Some("bobs-contact@example.org".to_string()),
            windows: vec![
                chat(ALICE, "contact@example.org"),
                chat(BOB, "bobs-contact@example.org"),
            ],
        };
        let mut tracker = tracker_with(&dir, &layout);
        tracker.begin_restore(ALICE);
        tracker.update(
            vec![chat(ALICE, "contact@example.org")],
            Some(CONSOLE_WINDOW.to_string()),
        );
        assert_eq!(on_disk(&dir), layout);
    }

    #[test]
    fn pending_active_waits_for_window() {
        let dir = tempfile::tempdir().unwrap();
        let room = "room@muc.example.org";
        let layout = Layout {
            active: Some(room.to_string()),
            windows: vec![chat(ALICE, room)],
        };
        let mut tracker = tracker_with(&dir, &layout);
        tracker.begin_restore(ALICE);
        let console = vec![CONSOLE_WINDOW.to_string()];
        assert_eq!(
            tracker.take_pending_active(&console, Some(CONSOLE_WINDOW)),
            None
        );
        let open = vec![CONSOLE_WINDOW.to_string(), room.to_string()];
        assert_eq!(
            tracker.take_pending_active(&open, Some(CONSOLE_WINDOW)),
            Some(room.to_string())
        );
        assert_eq!(
            tracker.take_pending_active(&open, Some(CONSOLE_WINDOW)),
            None
        );
    }

    #[test]
    fn user_navigation_cancels_pending_active() {
        let dir = tempfile::tempdir().unwrap();
        let room = "room@muc.example.org";
        let layout = Layout {
            active: Some(room.to_string()),
            windows: vec![chat(ALICE, room)],
        };
        let mut tracker = tracker_with(&dir, &layout);
        tracker.begin_restore(ALICE);
        let open = vec![
            CONSOLE_WINDOW.to_string(),
            "other@example.org".to_string(),
            room.to_string(),
        ];
        assert_eq!(
            tracker.take_pending_active(&open, Some("other@example.org")),
            None
        );
        assert_eq!(
            tracker.take_pending_active(&open, Some(CONSOLE_WINDOW)),
            None
        );
    }
}
