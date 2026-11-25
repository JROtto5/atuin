use std::borrow::Cow;
use std::path::PathBuf;
use std::str;

use async_trait::async_trait;
use atuin_common::path::PathBufExt as _;
use atuin_common::userio;
use eyre::{Result, eyre};
use jiff::{SignedDuration, Timestamp};

use super::{Importer, Loader, get_histfile_path, unix_byte_lines};
use crate::history::History;
use crate::import::read_to_end;

#[derive(Debug)]
pub struct Bash {
    bytes: Vec<u8>,
}

fn default_histpath() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().ok_or_else(|| eyre!("could not find home directory"))?;

    Ok(home_dir.joined(".bash_history"))
}

#[async_trait]
impl Importer for Bash {
    const NAME: &'static str = "bash";

    async fn new() -> Result<Self> {
        let bytes = read_to_end(get_histfile_path(default_histpath)?)?;
        Ok(Self { bytes })
    }

    async fn entries(&mut self) -> Result<usize> {
        let count = parse_lines(&self.bytes)
            .iter()
            .filter(|line| matches!(line, LineType::Command(_)))
            .count();
        Ok(count)
    }

    async fn load(self, h: &mut impl Loader) -> Result<()> {
        let lines = parse_lines(&self.bytes);

        let (commands_before_first_timestamp, first_timestamp) = lines
            .iter()
            .enumerate()
            .find_map(|(i, line)| match line {
                LineType::Timestamp(t) => Some((i, *t)),
                _ => None,
            })
            // if no known timestamps, use now as base
            .unwrap_or_else(|| (lines.len(), Timestamp::now()));

        // if no timestamp is recorded, then use this increment to set an arbitrary timestamp
        // to preserve ordering
        // this increment is deliberately very small to prevent particularly fast fingers
        // causing ordering issues; it also helps in handling the "here document" syntax
        // in a file that doesn't start with a timestamp, where several lines are recorded in
        // succession without individual timestamps
        let timestamp_increment = SignedDuration::from_millis(1);

        // make sure there is a minimum amount of time before the first known timestamp
        // to fit all commands, given the default increment
        let backfill = timestamp_increment
            * i32::try_from(commands_before_first_timestamp).unwrap_or(i32::MAX);
        // a timestamp near the start of the representable range would underflow
        let mut next_timestamp = first_timestamp.checked_sub(backfill).unwrap_or(first_timestamp);

        for line in lines {
            match line {
                LineType::NotUtf8 => unreachable!(), // already filtered
                LineType::Empty => {}                // do nothing
                LineType::Timestamp(t) => {
                    if t < next_timestamp {
                        userio::log::warn!("import-bash-time-reversal");
                    }
                    next_timestamp = t;
                }
                LineType::Command(c) => {
                    let imported =
                        History::import().shell("bash").timestamp(next_timestamp).command(c);

                    h.push(imported.build().into()).await?;
                    // a timestamp near the end of the representable range would overflow
                    next_timestamp =
                        next_timestamp.checked_add(timestamp_increment).unwrap_or(next_timestamp);
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
enum LineType<'a> {
    NotUtf8,
    /// Can happen when using the "here document" syntax.
    Empty,
    /// A timestamp line start with a '#', followed immediately by an integer
    /// that represents seconds since UNIX epoch.
    Timestamp(Timestamp),
    /// Anything else.
    Command(Cow<'a, str>),
}
impl<'a> From<&'a [u8]> for LineType<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        let Ok(line) = str::from_utf8(bytes) else {
            return LineType::NotUtf8;
        };
        if line.is_empty() {
            return LineType::Empty;
        }

        match try_parse_line_as_timestamp(line) {
            Some(time) => LineType::Timestamp(time),
            None => LineType::Command(Cow::Borrowed(line)),
        }
    }
}

/// Splits the history file into timestamps and commands, the way bash reads it back.
///
/// When the file starts with a timestamp and `HISTTIMEFORMAT` is set, bash reads every line up
/// to the next timestamp as one command. Bash writes a command over several lines when it
/// contains a here-document, or for any multi-line command with `shopt -s lithist`. The
/// importer assumes `HISTTIMEFORMAT` was set because that setting is what writes the
/// timestamps.
fn parse_lines(bytes: &[u8]) -> Vec<LineType<'_>> {
    let mut lines = unix_byte_lines(bytes).map(LineType::from).peekable();
    if !matches!(lines.peek(), Some(LineType::Timestamp(_))) {
        // invalid utf8 lines are ignored
        return lines.filter(|line| !matches!(line, LineType::NotUtf8)).collect();
    }

    let mut parsed = Vec::new();
    let mut entry = Vec::new();
    let mut valid = true;
    for line in lines {
        match line {
            LineType::Timestamp(_) => {
                if valid {
                    push_entry(&mut parsed, &entry);
                }
                entry.clear();
                valid = true;
                parsed.push(line);
            }
            // an entry with an invalid utf8 line is ignored as a whole
            LineType::NotUtf8 => valid = false,
            LineType::Empty => entry.push(Cow::Borrowed("")),
            LineType::Command(c) => entry.push(c),
        }
    }
    if valid {
        push_entry(&mut parsed, &entry);
    }
    parsed
}

/// Adds the lines of one timestamped entry as a single command, without blank lines at either end.
fn push_entry<'a>(parsed: &mut Vec<LineType<'a>>, entry: &[Cow<'a, str>]) {
    let Some(first) = entry.iter().position(|line| !line.is_empty()) else {
        return;
    };
    let last = entry.iter().rposition(|line| !line.is_empty()).unwrap_or(first);
    let command = match &entry[first..=last] {
        [line] => line.clone(),
        lines => Cow::Owned(lines.join("\n")),
    };
    parsed.push(LineType::Command(command));
}

fn try_parse_line_as_timestamp(line: &str) -> Option<Timestamp> {
    let seconds = line.strip_prefix('#')?.parse().ok()?;
    Timestamp::from_second(seconds).ok()
}

#[cfg(test)]
mod test {
    use std::cmp::Ordering;

    use itertools::{Itertools, assert_equal};
    use rstest::rstest;

    use super::Bash;
    use crate::import::Importer;
    use crate::import::tests::TestLoader;

    #[rstest]
    #[case::no_timestamps(
        "cargo install atuin\ncargo update\ncargo :b̷i̶t̴r̵o̴t̴ ̵i̷s̴ ̷r̶e̵a̸l̷\n"
            .as_bytes()
            .to_owned(),
        3,
        vec![
            "cargo install atuin",
            "cargo update",
            "cargo :b̷i̶t̴r̵o̴t̴ ̵i̷s̴ ̷r̶e̵a̸l̷",
        ]
    )]
    #[case::partial_timestamps(
        b"git reset\n#1672919006\ngit clean -dxf\ncd ../\n".to_vec(),
        3,
        vec!["git reset", "git clean -dxf", "cd ../"]
    )]
    #[tokio::test]
    async fn parse_strictly_sorted(
        #[case] bytes: Vec<u8>,
        #[case] expected_entries: usize,
        #[case] expected_commands: Vec<&str>,
    ) {
        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), expected_entries);

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(loader.buf.iter().map(|h| h.command.as_str()), expected_commands);
        assert!(is_strictly_sorted(loader.buf.iter().map(|h| h.timestamp)));
    }

    #[rstest]
    #[tokio::test]
    async fn parse_with_timestamps() {
        let bytes = b"#1672918999
git reset
#1672919006
git clean -dxf
#1672919020
cd ../
"
        .to_vec();

        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), 3);

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(loader.buf.iter().map(|h| h.command.as_str()), [
            "git reset",
            "git clean -dxf",
            "cd ../",
        ]);
        assert_equal(loader.buf.iter().map(|h| h.timestamp.as_second()), [
            1_672_918_999,
            1_672_919_006,
            1_672_919_020,
        ]);
    }

    #[rstest]
    #[case::here_document(
        b"#1672918999\ncat <<END\nhello\nEND\n\n#1672919006\necho after\n".to_vec(),
        vec!["cat <<END\nhello\nEND", "echo after"],
        vec![1_672_918_999, 1_672_919_006]
    )]
    #[case::lithist_loop(
        b"#1672918999\nfor i in 1 2\ndo\n\n  echo $i\ndone\n#1672919006\necho after\n".to_vec(),
        vec!["for i in 1 2\ndo\n\n  echo $i\ndone", "echo after"],
        vec![1_672_918_999, 1_672_919_006]
    )]
    #[case::blank_line_after_timestamp(
        b"#1672918999\n\ngit reset\n#1672919006\necho after\n".to_vec(),
        vec!["git reset", "echo after"],
        vec![1_672_918_999, 1_672_919_006]
    )]
    // a shell without HISTTIMEFORMAT appended the last two lines, and bash with
    // HISTTIMEFORMAT set reads them as part of the entry before them
    #[case::untimestamped_tail(
        b"#1672918999\ngit reset\n#1672919006\necho after\nls\npwd\n".to_vec(),
        vec!["git reset", "echo after\nls\npwd"],
        vec![1_672_918_999, 1_672_919_006]
    )]
    #[case::invalid_utf8_line(
        b"#1672918999\ncat <<END\nhel\xffo\nEND\n#1672919006\necho after\n".to_vec(),
        vec!["echo after"],
        vec![1_672_919_006]
    )]
    #[tokio::test]
    async fn parse_multiline_entries(
        #[case] bytes: Vec<u8>,
        #[case] expected_commands: Vec<&str>,
        #[case] expected_seconds: Vec<i64>,
    ) {
        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), expected_commands.len());

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(loader.buf.iter().map(|h| h.command.as_str()), expected_commands);
        assert_equal(loader.buf.iter().map(|h| h.timestamp.as_second()), expected_seconds);
    }

    fn is_strictly_sorted<T>(iter: impl IntoIterator<Item = T>) -> bool
    where
        T: Clone + PartialOrd,
    {
        iter.into_iter()
            .tuple_windows()
            .all(|(a, b)| matches!(a.partial_cmp(&b), Some(Ordering::Less)))
    }

    #[rstest]
    #[tokio::test]
    async fn timestamp_near_range_start_does_not_panic_on_backfill() {
        // first timestamp is near the minimum representable instant, preceded by an
        // untimestamped command; backfilling before it must not underflow
        let bytes = b"cargo install atuin
#-377705023201
cargo update
"
        .to_vec();

        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), 2);

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(loader.buf.iter().map(|h| h.command.as_str()), [
            "cargo install atuin",
            "cargo update",
        ]);
    }

    #[rstest]
    #[tokio::test]
    async fn timestamp_near_range_end_does_not_panic_on_increment() {
        // first timestamp is the maximum representable instant (253402207200 is the
        // last second `Timestamp` can represent, i.e. 9999-12-30 22:00:00 UTC).
        // 1000 untimestamped commands walk the 1ms increment past .999 and off the
        // end of the representable range.
        // the untimestamped first line keeps bash from reading the rest as one command
        let commands: Vec<String> = (0..=1_000).map(|i| format!("cmd-{i}")).collect();
        let bytes =
            format!("{}\n#253402207200\n{}\n", commands[0], commands[1..].join("\n")).into_bytes();

        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), commands.len());

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(
            loader.buf.iter().map(|h| h.command.as_str()),
            commands.iter().map(String::as_str),
        );

        // the increment saturates at the maximum representable instant rather than
        // wrapping or resetting to the epoch
        assert_eq!(loader.buf.last().unwrap().timestamp.as_second(), 253_402_207_200);
    }
}
