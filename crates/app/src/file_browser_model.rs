//! Pure file-browser state rules. The GPUI entity stays thin; these rules are
//! deliberately independent of the engine so stale replies and path edge cases
//! can be tested without a server or a display.

#[cfg(test)]
use std::path::PathBuf;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub file: bool,
    pub symlink: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Missing,
    NotDir,
    Denied,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listing {
    pub path: String,
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub status: Option<Status>,
}

pub(crate) fn parse_listing(value: Value) -> Result<Listing, &'static str> {
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or("Directory reply has no path; refresh or reconnect the server")?
        .to_owned();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| *status != "ok")
        .map(|status| match status {
            "missing" => Status::Missing,
            "not_dir" => Status::NotDir,
            "denied" => Status::Denied,
            other => Status::Other(other.to_owned()),
        });
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name")?.as_str()?;
                    if name.is_empty()
                        || name == "."
                        || name == ".."
                        || name.contains('/')
                        || name.chars().any(char::is_control)
                    {
                        return None;
                    }
                    Some(Entry {
                        name: name.to_owned(),
                        file: entry.get("file").and_then(Value::as_bool).unwrap_or(false),
                        symlink: entry
                            .get("symlink")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut entries = entries;
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Listing {
        path,
        entries,
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        status,
    })
}

#[derive(Clone, Copy, Default)]
pub(crate) struct FilterOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

pub(crate) fn filter_with_options(
    entries: &[Entry],
    needle: &str,
    options: FilterOptions,
) -> Result<Vec<Entry>, String> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(entries.to_vec());
    }
    let pattern = if options.regex {
        needle.to_owned()
    } else {
        regex::escape(needle)
    };
    let pattern = if options.whole_word {
        format!(r"(?:^|\W)(?:{pattern})(?:$|\W)")
    } else {
        pattern
    };
    let matcher = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| format!("正则表达式无效：{e}"))?;
    Ok(entries
        .iter()
        .filter(|entry| matcher.is_match(&entry.name))
        .cloned()
        .collect())
}

pub(crate) fn filter_prefix(needle: &str) -> String {
    needle.trim().to_owned()
}

pub(crate) fn join_path(parent: &str, child: &str) -> String {
    let child = child.trim_matches('/');
    if child.is_empty() {
        return if parent.is_empty() {
            String::new()
        } else {
            parent.to_owned()
        };
    }
    if parent.is_empty() || parent == "/" {
        return format!("/{child}");
    }
    format!("{}/{child}", parent.trim_end_matches('/'))
}

pub(crate) fn up_path(path: &str) -> String {
    if path == "/" {
        return "/".into();
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return String::new();
    }
    match path.rsplit_once('/') {
        Some(("", _)) => "/".to_owned(),
        Some((parent, _)) => parent.to_owned(),
        None => String::new(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestStamp {
    pub generation: u64,
    pub target: String,
}

impl RequestStamp {
    pub(crate) fn accepts(&self, reply: &Self) -> bool {
        self.generation == reply.generation && self.target == reply.target
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobState {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone)]
pub(crate) struct Job<T, K, P> {
    pub target: T,
    pub label: String,
    pub kind: K,
    pub state: JobState,
    pub error: Option<String>,
    pub progress: Option<P>,
    pub id: usize,
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<T, K, P> Job<T, K, P> {
    pub(crate) fn cancel(&mut self) {
        match self.state {
            JobState::Queued => self.state = JobState::Cancelled,
            JobState::Running => self
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed),
            _ => {}
        }
    }

    pub(crate) fn retry(&mut self) -> bool {
        if !matches!(self.state, JobState::Failure | JobState::Cancelled) {
            return false;
        }
        self.state = JobState::Queued;
        self.error = None;
        self.progress = None;
        self.cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        true
    }
}

pub(crate) fn next_job<T, K, P>(jobs: &mut [Job<T, K, P>]) -> Option<&mut Job<T, K, P>> {
    if jobs.iter().any(|job| job.state == JobState::Running) {
        return None;
    }
    let job = jobs.iter_mut().find(|job| job.state == JobState::Queued)?;
    job.state = JobState::Running;
    Some(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filter_controls_match_case_words_and_regular_expressions() {
        let entries: Vec<_> = [
            "README.md",
            "readme.txt",
            "src",
            "src-backup",
            "src_utils",
            "other-src.txt",
        ]
        .into_iter()
        .map(|name| Entry {
            name: name.into(),
            file: true,
            symlink: false,
        })
        .collect();
        let names = |query, options| {
            filter_with_options(&entries, query, options)
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names("readme", FilterOptions::default()),
            ["README.md", "readme.txt"]
        );
        assert_eq!(
            names(
                "readme",
                FilterOptions {
                    case_sensitive: true,
                    ..Default::default()
                }
            ),
            ["readme.txt"]
        );
        assert_eq!(
            names(
                "src",
                FilterOptions {
                    whole_word: true,
                    ..Default::default()
                }
            ),
            ["src", "src-backup", "other-src.txt"]
        );
        assert_eq!(
            names(
                r"^readme\.(md|txt)$",
                FilterOptions {
                    regex: true,
                    ..Default::default()
                }
            ),
            ["README.md", "readme.txt"]
        );
        assert!(filter_with_options(
            &entries,
            "[",
            FilterOptions {
                regex: true,
                ..Default::default()
            }
        )
        .is_err());
        assert!(
            names("[", FilterOptions::default()).is_empty(),
            "literal punctuation is not regex syntax"
        );
        assert_eq!(
            names(
                "",
                FilterOptions {
                    whole_word: true,
                    regex: true,
                    ..Default::default()
                }
            )
            .len(),
            entries.len()
        );
    }

    #[test]
    fn parses_and_sorts_entries_without_inventing_metadata() {
        let listing = parse_listing(json!({
            "path": "/work",
            "entries": [
                {"name":"z.txt", "file":true},
                {"name":"src", "file":false},
                {"name":"link", "file":false, "symlink":true},
            ],
            "truncated": true
        }))
        .unwrap();
        assert_eq!(listing.path, "/work");
        assert!(listing.truncated);
        assert_eq!(
            listing.entries[0],
            Entry {
                name: "link".into(),
                file: false,
                symlink: true
            }
        );
        assert_eq!(
            listing.entries[1],
            Entry {
                name: "src".into(),
                file: false,
                symlink: false
            }
        );
        assert_eq!(
            listing.entries[2],
            Entry {
                name: "z.txt".into(),
                file: true,
                symlink: false
            }
        );
    }

    #[test]
    fn status_is_visible_instead_of_becoming_an_empty_directory() {
        for (status, expected) in [
            ("missing", Status::Missing),
            ("not_dir", Status::NotDir),
            ("denied", Status::Denied),
        ] {
            let listing =
                parse_listing(json!({"path":"/work", "status": status, "entries": []})).unwrap();
            assert_eq!(listing.status, Some(expected));
        }
    }

    #[test]
    fn filtering_is_local_and_prefix_filter_is_explicit() {
        let entries = vec![
            Entry {
                name: "alpha.txt".into(),
                file: true,
                symlink: false,
            },
            Entry {
                name: "beta.txt".into(),
                file: true,
                symlink: false,
            },
        ];
        assert_eq!(
            filter_with_options(&entries, "ALP", FilterOptions::default()).unwrap()[0].name,
            "alpha.txt"
        );
        assert_eq!(filter_prefix(" needle "), "needle");
    }

    #[test]
    fn paths_join_under_root_and_up_never_escapes_root() {
        assert_eq!(join_path("/", "child"), "/child");
        assert_eq!(join_path("/work", "child"), "/work/child");
        assert_eq!(up_path("/work/child"), "/work");
        assert_eq!(up_path("/"), "/");
        assert_eq!(up_path(""), "");
    }

    #[test]
    fn stale_generation_or_target_cannot_apply_reply() {
        let current = RequestStamp {
            generation: 2,
            target: "server/session/block".into(),
        };
        assert!(!current.accepts(&RequestStamp {
            generation: 1,
            target: current.target.clone()
        }));
        assert!(!current.accepts(&RequestStamp {
            generation: 2,
            target: "other".into()
        }));
        assert!(current.accepts(&current));
    }

    fn upload(target: &str) -> Job<String, Vec<PathBuf>, ()> {
        Job {
            target: target.into(),
            label: target.into(),
            kind: vec![PathBuf::from("a")],
            state: JobState::Queued,
            error: None,
            progress: None,
            id: 0,
            cancel: std::sync::Arc::default(),
        }
    }

    #[test]
    fn queued_jobs_keep_original_target_and_run_one_at_a_time() {
        let mut jobs = vec![upload("original"), upload("other-server")];
        assert_eq!(next_job(&mut jobs).unwrap().target, "original");
        assert!(next_job(&mut jobs).is_none());
        jobs[0].state = JobState::Success;
        assert_eq!(next_job(&mut jobs).unwrap().target, "other-server");
        assert_eq!(jobs[0].target, "original");
    }

    #[test]
    fn cancel_waits_for_running_task_cleanup_before_retry_or_next_job() {
        let mut jobs = vec![upload("one"), upload("two")];
        next_job(&mut jobs).unwrap().cancel();
        assert!(!jobs[0].retry());
        assert!(next_job(&mut jobs).is_none());
        assert!(jobs[0].cancel.load(std::sync::atomic::Ordering::Relaxed));
        jobs[1].cancel();
        assert_eq!(jobs[1].state, JobState::Cancelled);
        jobs[0].state = JobState::Cancelled;
        assert!(jobs[0].retry());
        assert!(!jobs[0].cancel.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn malformed_listing_and_unsafe_names_are_not_navigable() {
        assert!(parse_listing(json!({"entries":[]})).is_err());
        let listing = parse_listing(json!({"path":"/work","status":"ok","entries":[
            {"name":".."}, {"name":"/etc"}, {"name":"bad\nname"}, {"name":"normal"}
        ]}))
        .unwrap();
        assert_eq!(listing.status, None);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "normal");
    }
}
