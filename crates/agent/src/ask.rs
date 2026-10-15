//! The Ask panel: the prompt a question becomes, and the citations in the answer.

use crate::summarize::parse_clock;

/// A moment an answer cites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cite {
    /// The meeting.
    pub meeting_id: String,
    /// Where, milliseconds.
    pub at_ms: i64,
}

/// What a question covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskScope {
    /// One meeting: its id and title.
    Meeting(String, String),
    /// Every meeting.
    All,
}

/// The prompt for a question. `history` is the conversation so far, as (question,
/// answer) pairs, oldest first.
/// `me` is the user's name as transcripts show it, so answers can say "you".
pub fn ask_prompt(
    question: &str,
    scope: &AskScope,
    history: &[(String, String)],
    me: Option<&str>,
) -> String {
    let scope = match scope {
        AskScope::Meeting(id, title) => format!(
            "The question is about the meeting \"{title}\" (meeting_id \"{id}\"); read it with \
             get_transcript and look further only if the question needs it."
        ),
        AskScope::All => "The question may be about any meeting; find them with \
                          search_meetings and list_meetings."
            .to_owned(),
    };
    let mut earlier = String::new();
    for (q, a) in history.iter().rev().take(4).rev() {
        earlier.push_str(&format!("Q: {q}\nA: {a}\n\n"));
    }
    if !earlier.is_empty() {
        earlier = format!("The conversation so far:\n\n{earlier}");
    }
    let me = me.map_or_else(String::new, |n| {
        format!(" The user is {n} in the transcripts; call them \"you\".")
    });
    format!(
        "You answer questions about the user's recorded meetings.{me} Use the switchscribe MCP \
tools (list_meetings, get_transcript, search_meetings, get_action_items) to read them; do \
not use any other tools. {scope}

{earlier}Question: {question}

Answer in a few plain sentences. After each claim, cite the moment it comes from as \
[[meeting_id@mm:ss]], using the meeting_id and line time from the tools, for example \
[[2026-10-15-1000-aks@03:56]]. If the user asks you to change a summary or create a Jira \
issue, call update_summary or create_jira_issue: the change waits for their approval, so \
say you proposed it."
    )
}

/// Split an answer into its text (citations removed) and the moments it cites, in order
/// and without repeats.
pub fn parse_answer(answer: &str) -> (String, Vec<Cite>) {
    let mut text = String::new();
    let mut cites: Vec<Cite> = Vec::new();
    let mut rest = answer;
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start..].find("]]") else {
            break;
        };
        let inner = &rest[start + 2..start + len];
        let cite = inner.rsplit_once('@').and_then(|(m, t)| {
            let m = m.trim();
            (!m.is_empty() && !m.contains(char::is_whitespace))
                .then_some(())
                .and(parse_clock(t))
                .map(|at_ms| Cite {
                    meeting_id: m.to_owned(),
                    at_ms,
                })
        });
        match cite {
            Some(c) => {
                text.push_str(rest[..start].trim_end_matches(' '));
                if !cites.contains(&c) {
                    cites.push(c);
                }
            }
            None => text.push_str(&rest[..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    text.push_str(rest);
    (text.trim().to_owned(), cites)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_become_chips() {
        let (text, cites) = parse_answer(
            "We cut over on the 22nd [[aks@03:56]]. Marcus wants a runbook [[aks@05:02]] \
             [[aks@03:56]]. See [[not a cite]].",
        );
        assert_eq!(
            text,
            "We cut over on the 22nd. Marcus wants a runbook. See [[not a cite]]."
        );
        assert_eq!(
            cites,
            [
                Cite {
                    meeting_id: "aks".into(),
                    at_ms: 236_000
                },
                Cite {
                    meeting_id: "aks".into(),
                    at_ms: 302_000
                }
            ]
        );
    }

    #[test]
    fn prompts_name_the_scope_and_history() {
        let p = ask_prompt(
            "And the owner?",
            &AskScope::Meeting("aks".into(), "AKS planning".into()),
            &[("What was decided?".into(), "Cut over.".into())],
            Some("Jeremie"),
        );
        assert!(p.contains("The user is Jeremie"));
        assert!(p.contains("meeting_id \"aks\""));
        assert!(p.contains("Q: What was decided?\nA: Cut over."));
        assert!(p.contains("Question: And the owner?"));
    }
}
