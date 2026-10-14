//! The rules file, `.verified/v.toml`: the grants you write in advance (A2).
//!
//! ```toml
//! # Allow rules only; anything no rule allows asks you.
//! [[allow]]
//! tool = "bash"          # read | edit | bash | spawn | send | wake | tasks | cancel
//! actor = "researcher"   # optional: this actor's own name; absent = every actor
//! prefix = "cargo test"  # optional; or `exact = "..."`; absent = any subject
//! ```
//!
//! The file is read through the [`Env`] seam at boot and before each of your
//! messages; there is no watching. It is parsed here, once, into the core's
//! [`Rules`], and recorded in the log with its text whenever it differs from
//! the text in force (`RulesLoaded`), so the log alone says which rules
//! granted what. With no file the one rule is the default, `read: allow`. A
//! file that does not parse, is too large, holds too many rules or would not
//! fit a log line grants nothing: only your live approvals do, and its text
//! is recorded so you can see why.

use verified_core::Loaded;
use verified_domain::{ActorName, Measured, Pattern, Rule, Rules, Tool};

use crate::env::Env;
use crate::limits::{RECORD_OVERHEAD, ServerLimits};
use crate::payload::{Parsed, measure};

/// The rules file's name, under the daemon's directory.
pub const FILE: &str = "v.toml";

/// The file as written.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    allow: Vec<Allow>,
}

/// One `[[allow]]` table.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Allow {
    tool: String,
    actor: Option<String>,
    exact: Option<String>,
    prefix: Option<String>,
}

/// The tool a rule names.
fn tool(name: &str) -> Result<Tool, String> {
    match name {
        "read" => Ok(Tool::Read),
        "edit" => Ok(Tool::Edit),
        "bash" => Ok(Tool::Bash),
        "spawn" => Ok(Tool::Spawn),
        "send" => Ok(Tool::Send),
        "wake" => Ok(Tool::Wake),
        "tasks" => Ok(Tool::Tasks),
        "cancel" => Ok(Tool::Cancel),
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// The rules `text` says, in file order, at most `count` of them.
///
/// # Errors
/// The text is not TOML of the file's shape, names an unknown tool or a bad
/// actor name, gives both `exact` and `prefix`, or holds more than `count`
/// rules. The error says which.
pub fn parse(text: &str, count: usize) -> Result<Rules, String> {
    let file: File = toml::from_str(text).map_err(|error| error.to_string())?;
    if file.allow.len() > count {
        return Err(format!("{} rules, more than {count}", file.allow.len()));
    }
    let mut rules = Vec::with_capacity(file.allow.len());
    for (at, allow) in file.allow.into_iter().enumerate() {
        let rule = |error: String| format!("rule {}: {error}", at.saturating_add(1));
        let subject = match (allow.exact, allow.prefix) {
            (None, None) => Pattern::Any,
            (Some(exact), None) => Pattern::Exact(exact.into_bytes()),
            (None, Some(prefix)) => Pattern::Prefix(prefix.into_bytes()),
            (Some(_), Some(_)) => {
                return Err(rule("`exact` and `prefix` together".to_owned()));
            }
        };
        let actor = allow
            .actor
            .map(|name| ActorName::parse(&name).map_err(|error| rule(error.to_string())))
            .transpose()?;
        rules.push(Rule {
            actor,
            tool: tool(&allow.tool).map_err(rule)?,
            subject,
        });
    }
    Ok(Rules { rules })
}

/// The rules in force for the file's `bytes` (`None`: there is no file),
/// and its text: what a `RulesLoaded` records.
#[must_use]
pub fn rules(bytes: Option<Vec<u8>>, limits: &ServerLimits) -> Loaded<Parsed> {
    let Some(bytes) = bytes else {
        return Loaded::defaults();
    };
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
    let rules = parse(&text, limits.rules_count).unwrap_or(Rules { rules: Vec::new() });
    let loaded = Loaded {
        rules,
        file: Some(measure(text)),
    };
    if fits(&loaded, limits) {
        loaded
    } else {
        // Recorded as too large: no rule, and a note in place of the text.
        Loaded {
            rules: Rules { rules: Vec::new() },
            file: Some(measure(format!(
                "# {FILE} is too large to record in the log: no rule grants anything"
            ))),
        }
    }
}

/// Whether `loaded`'s record fits one log line.
fn fits(loaded: &Loaded<Parsed>, limits: &ServerLimits) -> bool {
    #[derive(serde::Serialize)]
    struct Record<'loaded> {
        rules: &'loaded Rules,
        file: Option<&'loaded Measured<String>>,
    }
    let record = Record {
        rules: &loaded.rules,
        file: loaded.file.as_ref(),
    };
    serde_json::to_vec(&record)
        .is_ok_and(|json| json.len().saturating_add(RECORD_OVERHEAD) <= limits.log_line_bytes)
}

/// Reads the rules file through `env`. A file v cannot read grants
/// nothing, as one that does not parse, and says why.
pub async fn load(env: &dyn Env, limits: &ServerLimits) -> Loaded<Parsed> {
    match env.rules_file(limits.rules_file_bytes).await {
        Ok(bytes) => rules(bytes, limits),
        Err(error) => Loaded {
            rules: Rules { rules: Vec::new() },
            file: Some(measure(format!("# {FILE} could not be read: {error}"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use verified_domain::{ActorName, Pattern, Rule, Rules, Tool};

    use super::{parse, rules};
    use crate::limits::ServerLimits;

    #[test]
    fn rules_parse_in_file_order() {
        let text = r#"
            [[allow]]
            tool = "bash"
            actor = "researcher"
            prefix = "cargo test"

            [[allow]]
            tool = "read"

            [[allow]]
            tool = "edit"
            exact = "notes.md"
        "#;
        assert_eq!(
            parse(text, 8),
            Ok(Rules {
                rules: vec![
                    Rule {
                        actor: Some(ActorName::parse("researcher").expect("a name")),
                        tool: Tool::Bash,
                        subject: Pattern::Prefix(b"cargo test".to_vec()),
                    },
                    Rule {
                        actor: None,
                        tool: Tool::Read,
                        subject: Pattern::Any,
                    },
                    Rule {
                        actor: None,
                        tool: Tool::Edit,
                        subject: Pattern::Exact(b"notes.md".to_vec()),
                    },
                ]
            })
        );
        assert_eq!(parse("", 8), Ok(Rules { rules: Vec::new() }), "no rules");
    }

    #[test]
    fn a_bad_file_grants_nothing() {
        for bad in [
            "[[allow]]\ntool = \"rm\"",
            "[[allow]]\ntool = \"bash\"\nexact = \"a\"\nprefix = \"b\"",
            "[[allow]]\ntool = \"bash\"\nactor = \"no spaces\"",
            "[[allow]]\ntool = \"bash\"\ncolour = \"red\"",
            "[[deny]]\ntool = \"bash\"",
            "not toml at all = = =",
        ] {
            assert!(parse(bad, 8).is_err(), "{bad}");
            let loaded = rules(Some(bad.as_bytes().to_vec()), &ServerLimits::v0());
            assert!(loaded.rules.rules.is_empty(), "{bad} grants nothing");
            assert_eq!(
                loaded.file.map(|file| file.body),
                Some(bad.to_owned()),
                "its text is recorded"
            );
        }
        let many = "[[allow]]\ntool = \"read\"\n".repeat(3);
        assert!(parse(&many, 2).is_err(), "too many rules");
    }

    #[test]
    fn no_file_is_the_default_rule() {
        let loaded = rules(None, &ServerLimits::v0());
        assert_eq!(loaded.rules, Rules::default());
        assert_eq!(loaded.file, None);
    }

    #[test]
    fn a_file_too_large_to_record_grants_nothing() {
        let mut limits = ServerLimits::v0();
        limits.log_line_bytes = 2_048;
        let text = format!(
            "[[allow]]\ntool = \"bash\"\nexact = \"{}\"",
            "x".repeat(4_000)
        );
        let loaded = rules(Some(text.into_bytes()), &limits);
        assert!(loaded.rules.rules.is_empty(), "nothing granted");
        assert!(
            loaded
                .file
                .is_some_and(|file| file.body.contains("too large")),
            "and it says so"
        );
    }
}
