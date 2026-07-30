//! Marker-delimited editing of files Suede does not own.
//!
//! Suede owns only the text between its markers; everything outside them is
//! preserved byte for byte, so a user's Sway config is never clobbered.

pub const BEGIN: &str = "# BEGIN SUEDE_CONFIG";
pub const END: &str = "# END SUEDE_CONFIG";

/// The Sway configuration Suede needs in place.
pub const SWAY_BLOCK_BODY: &str = "\
# Managed by Suede. Edits inside this block are overwritten.
# Hand the session environment to systemd so the user service can reach Sway.
exec systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK
exec systemctl --user start sway-session.target
# An appliance shows content, not window decorations.
default_border none
default_floating_border none";

/// Whether the managed block is present.
pub fn has_block(text: &str) -> bool {
    find_block(text).is_some()
}

/// Insert or replace the managed block, leaving all other content untouched.
pub fn upsert_block(text: &str, body: &str) -> String {
    let block = format!("{BEGIN}\n{}\n{END}", body.trim_end_matches('\n'));

    match find_block(text) {
        Some((start, end)) => {
            let mut result = String::with_capacity(text.len() + block.len());
            result.push_str(&text[..start]);
            result.push_str(&block);
            result.push_str(&text[end..]);
            result
        }
        None => {
            let mut result = String::from(text);
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&block);
            result.push('\n');
            result
        }
    }
}

/// Remove the managed block, leaving all other content untouched.
pub fn remove_block(text: &str) -> String {
    match find_block(text) {
        Some((start, end)) => {
            // Also drop the blank separator line inserted ahead of the block,
            // so removing is an exact inverse of adding.
            let head = match text[..start].strip_suffix("\n\n") {
                Some(trimmed) => format!("{trimmed}\n"),
                None => text[..start].to_string(),
            };
            let tail = text[end..].strip_prefix('\n').unwrap_or(&text[end..]);
            format!("{head}{tail}")
        }
        None => text.to_string(),
    }
}

/// Byte range of the managed block, from the start of `BEGIN` to the end of `END`.
fn find_block(text: &str) -> Option<(usize, usize)> {
    let start = text.find(BEGIN)?;
    let end_marker = text[start..].find(END)? + start;
    Some((start, end_marker + END.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_CONFIG: &str = "set $mod Mod4\nbindsym $mod+Return exec alacritty\n";

    #[test]
    fn adds_the_block_to_an_existing_config() {
        let result = upsert_block(USER_CONFIG, SWAY_BLOCK_BODY);
        assert!(result.starts_with(USER_CONFIG));
        assert!(has_block(&result));
        assert!(result.contains("sway-session.target"));
    }

    #[test]
    fn preserves_user_content_exactly() {
        let result = upsert_block(USER_CONFIG, SWAY_BLOCK_BODY);
        let restored = remove_block(&result);
        assert_eq!(restored, USER_CONFIG);
    }

    #[test]
    fn preserves_content_on_both_sides_of_the_block() {
        let original = format!("before\n\n{BEGIN}\nold body\n{END}\nafter\n");
        let result = upsert_block(&original, "new body");
        assert!(result.starts_with("before\n"));
        assert!(result.ends_with("after\n"));
        assert!(result.contains("new body"));
        assert!(!result.contains("old body"));
    }

    #[test]
    fn is_idempotent() {
        let once = upsert_block(USER_CONFIG, SWAY_BLOCK_BODY);
        let twice = upsert_block(&once, SWAY_BLOCK_BODY);
        assert_eq!(once, twice);
    }

    #[test]
    fn never_duplicates_the_block() {
        let mut text = USER_CONFIG.to_string();
        for _ in 0..3 {
            text = upsert_block(&text, SWAY_BLOCK_BODY);
        }
        assert_eq!(text.matches(BEGIN).count(), 1);
        assert_eq!(text.matches(END).count(), 1);
    }

    #[test]
    fn works_on_an_empty_file() {
        let result = upsert_block("", SWAY_BLOCK_BODY);
        assert!(result.starts_with(BEGIN));
        assert!(has_block(&result));
        assert_eq!(remove_block(&result), "");
    }

    #[test]
    fn handles_a_file_without_a_trailing_newline() {
        let result = upsert_block("set $mod Mod4", SWAY_BLOCK_BODY);
        assert!(result.contains("set $mod Mod4\n"));
        assert!(has_block(&result));
    }

    #[test]
    fn removing_an_absent_block_changes_nothing() {
        assert_eq!(remove_block(USER_CONFIG), USER_CONFIG);
    }

    #[test]
    fn detects_absence() {
        assert!(!has_block(USER_CONFIG));
    }
}
