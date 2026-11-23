//! Shared command-line parsing.

use std::path::PathBuf;

pub type AnchorRect = crate::layout::Rect;

fn flag_tail<'a>(args: &'a [String], name: &str) -> Option<&'a [String]> {
    let index = args.iter().position(|arg| arg == name)?;
    Some(&args[index + 1..])
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    flag_tail(args, name)?.first().map(String::as_str)
}

pub fn parse_anchor(args: &[String]) -> Option<AnchorRect> {
    let mut iter = flag_tail(args, "--anchor")?.iter().map(String::as_str);
    let mut nums = [0f32; 4];
    for slot in &mut nums {
        *slot = iter.next()?.trim().parse().ok()?;
    }
    if !nums.iter().all(|n| n.is_finite()) {
        return None;
    }
    let [left, top, right, bottom] = nums;
    (right > left && bottom > top).then_some(AnchorRect {
        left,
        top,
        right,
        bottom,
    })
}

pub fn parse_anchor_monitor(args: &[String]) -> Option<u64> {
    let raw: u64 = flag_value(args, "--anchor-monitor")?.trim().parse().ok()?;
    (raw != 0).then_some(raw)
}

pub fn parse_parent_pid(args: &[String]) -> Option<u32> {
    let pid: u32 = flag_value(args, "--parent-pid")?.parse().ok()?;
    (pid != 0).then_some(pid)
}

pub fn parse_ready_file(args: &[String]) -> Option<PathBuf> {
    let path = flag_value(args, "--ready-file")?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn anchor_parses_four_numbers() {
        let a = parse_anchor(&args(&["--anchor", "10", "20", "110", "60"])).unwrap();
        assert_eq!(a.left, 10.0);
        assert_eq!(a.top, 20.0);
        assert_eq!(a.right, 110.0);
        assert_eq!(a.bottom, 60.0);
    }

    #[test]
    fn anchor_skips_unrelated_flags() {
        let a = parse_anchor(&args(&[
            "--parent-pid",
            "42",
            "--anchor",
            "1.5",
            "2.5",
            "3.5",
            "4.5",
        ]))
        .unwrap();
        assert_eq!(a.left, 1.5);
        assert_eq!(a.bottom, 4.5);
    }

    #[test]
    fn anchor_rejects_missing_or_short() {
        assert!(parse_anchor(&args(&[])).is_none());
        assert!(parse_anchor(&args(&["--anchor", "1", "2", "3"])).is_none());
    }

    #[test]
    fn anchor_rejects_malformed_numbers() {
        assert!(parse_anchor(&args(&["--anchor", "1", "x", "3", "4"])).is_none());
        assert!(parse_anchor(&args(&["--anchor", "NaN", "2", "3", "4"])).is_none());
    }

    #[test]
    fn anchor_rejects_degenerate_rectangles() {
        assert!(parse_anchor(&args(&["--anchor", "5", "5", "5", "9"])).is_none());
        assert!(parse_anchor(&args(&["--anchor", "5", "5", "9", "5"])).is_none());
        assert!(parse_anchor(&args(&["--anchor", "9", "9", "5", "5"])).is_none());
    }

    #[test]
    fn anchor_is_the_shared_rect_type() {
        let a = parse_anchor(&args(&["--anchor", "10", "20", "110", "60"])).unwrap();
        assert_eq!(a.width(), 100.0);
        assert_eq!(a.height(), 40.0);
        assert_eq!(a, crate::layout::Rect::new(10.0, 20.0, 110.0, 60.0));
        let placed = crate::layout::anchor_origin(
            a,
            80.0,
            30.0,
            crate::layout::Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
        );
        assert_eq!(placed, (20.0, 68.0));
    }

    #[test]
    fn parent_pid_parses_and_rejects() {
        assert_eq!(
            parse_parent_pid(&args(&["--parent-pid", "4242"])),
            Some(4242)
        );
        assert!(parse_parent_pid(&args(&["--parent-pid"])).is_none());
        assert!(parse_parent_pid(&args(&["--parent-pid", "x"])).is_none());
        assert!(parse_parent_pid(&args(&["--parent-pid", "0"])).is_none());
        assert!(
            parse_parent_pid(&args(&["--parent-pid", " 4242"])).is_none(),
            "the indicator formats this value itself, so padding stays rejected"
        );
        assert!(parse_parent_pid(&args(&["--anchor", "1", "2", "3", "4"])).is_none());
    }

    #[test]
    fn anchor_monitor_parses_and_rejects_zero() {
        assert_eq!(
            parse_anchor_monitor(&args(&["--anchor-monitor", "12345"])),
            Some(12345)
        );
        assert!(parse_anchor_monitor(&args(&["--anchor-monitor", "0"])).is_none());
        assert!(parse_anchor_monitor(&args(&["--anchor-monitor", "-1"])).is_none());
        assert!(parse_anchor_monitor(&args(&["--anchor-monitor"])).is_none());
        assert!(parse_anchor_monitor(&args(&["--anchor", "1", "2", "3", "4"])).is_none());
    }

    #[test]
    fn ready_file_parses_and_rejects_empty() {
        assert_eq!(
            parse_ready_file(&args(&[
                "--parent-pid",
                "42",
                "--ready-file",
                "/tmp/sysvitals/history-window.ready"
            ])),
            Some(PathBuf::from("/tmp/sysvitals/history-window.ready"))
        );
        assert!(parse_ready_file(&args(&["--ready-file"])).is_none());
        assert!(parse_ready_file(&args(&["--ready-file", "  "])).is_none());
        assert!(parse_ready_file(&args(&["--anchor", "1", "2", "3", "4"])).is_none());
    }
}
