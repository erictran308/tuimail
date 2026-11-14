//! Conversations: the list's messages put in threads by what answers what
//! (Message-ID, In-Reply-To, References), the conversation with the newest
//! message first, each in the order it was written, replies under what
//! they answer. Done here, from the headers in the cache, so it's the same
//! for every server, and for POP3.
//!
//! Those headers are the sender's: anyone can make a message look like a
//! reply in a conversation whose Message-ID they know. Every row still
//! says who it's from, and the reader shows the address first.

use std::collections::HashMap;

use crate::mail::Summary;

/// Replies deeper than this are drawn at this depth.
pub const MAX_DEPTH: usize = 6;
/// A message this far down a conversation starts another: checking that a
/// reply doesn't answer itself walks up this far at most.
const MAX_CHAIN: usize = 1000;

/// Where a message goes in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Its place in what was given.
    pub index: usize,
    /// The lines that show where it is in its conversation: `├─`, `└─`,
    /// `│ ` and spaces, two columns a level; empty for a conversation's
    /// first message.
    pub tree: String,
    /// It answers a message with the same subject (`Re: …`): the subject
    /// needn't show again.
    pub same_subject: bool,
}

/// Messages connected to the first `listed` by their conversation headers.
/// Shared references count even when the message they name isn't cached.
pub fn connected(summaries: &[&Summary], listed: usize) -> Vec<bool> {
    fn ids(s: &Summary) -> impl Iterator<Item = &str> {
        std::iter::once(s.message_id.as_str())
            .chain(std::iter::once(s.in_reply_to.as_str()))
            .chain(s.references.split_whitespace())
            .filter(|id| !id.is_empty())
    }
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, summary) in summaries.iter().enumerate().skip(listed) {
        for id in ids(summary) {
            by_id.entry(id).or_default().push(i);
        }
    }
    let mut keep = vec![false; summaries.len()];
    let mut pending: Vec<usize> = (0..listed).collect();
    keep[..listed].fill(true);
    while let Some(i) = pending.pop() {
        for id in ids(summaries[i]) {
            // Each header id is visited once, even in long conversations.
            if let Some(messages) = by_id.remove(id) {
                for j in messages {
                    if !keep[j] {
                        keep[j] = true;
                        pending.push(j);
                    }
                }
            }
        }
    }
    keep
}

/// `summaries` as threads: each conversation in turn, newest first.
#[cfg(test)]
pub fn arrange(summaries: &[&Summary]) -> Vec<Placed> {
    arrange_arrived(summaries, &[])
}

/// As [`arrange`], knowing where each message is and its UID there
/// (`arrived`: a number for its account and folder, and its UID): of
/// messages with one Message-ID in one folder, the one that came first is
/// the one replies go under, not one a sender dated later.
pub fn arrange_arrived(summaries: &[&Summary], arrived: &[(usize, u32)]) -> Vec<Placed> {
    let n = summaries.len();
    // A Message-ID's first message: a copy (in two inboxes) stands alone.
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (i, s) in summaries.iter().enumerate() {
        if s.message_id.is_empty() {
            continue;
        }
        let first = by_id.entry(s.message_id.as_str()).or_insert(i);
        if let (Some(&(place, uid)), Some(&(was_place, was_uid))) =
            (arrived.get(i), arrived.get(*first))
            && place == was_place
            && uid < was_uid
        {
            *first = i;
        }
    }
    // Each message answers the nearest one it names that's here: the one
    // it replies to, else the latest of its references.
    let mut parent: Vec<Option<usize>> = vec![None; n];
    for (i, s) in summaries.iter().enumerate() {
        let named =
            std::iter::once(s.in_reply_to.as_str()).chain(s.references.split_whitespace().rev());
        for id in named.filter(|id| !id.is_empty()) {
            if let Some(&p) = by_id.get(id)
                && p != i
                && !answers(&parent, p, i)
            {
                parent[i] = Some(p);
                break;
            }
        }
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, p) in parent.iter().enumerate() {
        match p {
            Some(p) => children[*p].push(i),
            None => roots.push(i),
        }
    }
    let date = |i: usize| summaries[i].date;
    for list in &mut children {
        list.sort_by_key(|&i| (date(i), i));
    }
    // A conversation is as new as its newest message.
    let mut newest = vec![i64::MIN; n];
    for &root in &roots {
        let mut stack = vec![root];
        let mut latest = i64::MIN;
        while let Some(i) = stack.pop() {
            latest = latest.max(date(i));
            stack.extend(&children[i]);
        }
        newest[root] = latest;
    }
    roots.sort_by_key(|&i| (std::cmp::Reverse(newest[i]), i));
    let subject = |i: usize| base_subject(&summaries[i].subject);
    let mut out = Vec::with_capacity(n);
    for root in roots {
        // Depth first; with each message, whether each level above it (and
        // it) has more to come below, for the lines.
        let mut stack: Vec<(usize, Vec<bool>)> = vec![(root, Vec::new())];
        while let Some((i, more)) = stack.pop() {
            let tree = tree(&more);
            let same_subject = parent[i].is_some_and(|p| subject(p) == subject(i));
            out.push(Placed {
                index: i,
                tree,
                same_subject,
            });
            let kids = &children[i];
            for (k, &child) in kids.iter().enumerate().rev() {
                let mut levels = more.clone();
                levels.push(k + 1 < kids.len());
                stack.push((child, levels));
            }
        }
    }
    out
}

/// Whether `from`, or what it answers, all the way up, is `message`: it
/// can't then answer `message` too. Past [`MAX_CHAIN`], taken as yes.
fn answers(parent: &[Option<usize>], from: usize, message: usize) -> bool {
    let mut at = Some(from);
    for _ in 0..MAX_CHAIN {
        match at {
            Some(i) if i == message => return true,
            Some(i) => at = parent[i],
            None => return false,
        }
    }
    true
}

/// The lines before a message: for each level above, `│ ` if more comes
/// there below, and its own `├─` (more answers to the same message come)
/// or `└─`.
fn tree(more: &[bool]) -> String {
    let Some((own, above)) = more.split_last() else {
        return String::new();
    };
    let above = &above[above.len().saturating_sub(MAX_DEPTH - 1)..];
    let mut out = String::new();
    for &more in above {
        out.push_str(if more { "│ " } else { "  " });
    }
    out.push_str(if *own { "├─" } else { "└─" });
    out
}

/// A subject without the `Re:` and `Fwd:` replies put before it.
pub fn base_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_lowercase();
        let Some(cut) = [
            "re:", "fwd:", "fw:", "aw:", "sv:", "wg:", "vs:", "antw:", "tr:",
        ]
        .iter()
        .find(|p| lower.starts_with(*p)) else {
            break;
        };
        s = s[cut.len()..].trim_start();
    }
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, reply_to: &str, refs: &str, date: i64, subject: &str) -> Summary {
        Summary {
            message_id: id.into(),
            in_reply_to: reply_to.into(),
            references: refs.into(),
            date,
            subject: subject.into(),
            ..Summary::default()
        }
    }

    /// The order, as (index, tree).
    fn order(list: &[Summary]) -> Vec<(usize, String)> {
        let refs: Vec<&Summary> = list.iter().collect();
        arrange(&refs)
            .into_iter()
            .map(|p| (p.index, p.tree))
            .collect()
    }

    #[test]
    fn replies_go_under_what_they_answer_and_the_newest_conversation_first() {
        let list = [
            summary("a", "", "", 10, "Plan"),
            summary("x", "", "", 30, "Lunch"),
            summary("b", "a", "a", 20, "Re: Plan"),
            summary("c", "a", "a", 40, "Re: Plan"),
            summary("d", "b", "a b", 50, "Re: Re: Plan"),
        ];
        assert_eq!(
            order(&list),
            [
                (0, "".into()),
                (2, "├─".into()),
                (4, "│ └─".into()),
                (3, "└─".into()),
                (1, "".into()),
            ]
        );
    }

    #[test]
    fn a_later_copy_of_a_message_id_doesnt_take_its_replies() {
        // The genuine message came first (UID 1); a sender's copy of its
        // Message-ID, dated later, is listed before it.
        let list = [
            summary("a", "", "", 90, "Forged"),
            summary("r", "a", "a", 20, "Re: Plan"),
            summary("a", "", "", 10, "Plan"),
        ];
        let refs: Vec<&Summary> = list.iter().collect();
        let placed = arrange_arrived(&refs, &[(0, 5), (0, 2), (0, 1)]);
        let reply = placed.iter().position(|p| p.index == 1).unwrap();
        assert_eq!(placed[reply - 1].index, 2, "under the first: {placed:?}");
    }

    #[test]
    fn a_missing_message_is_skipped_to_the_one_before() {
        // d answers b, which isn't here: under a.
        let list = [
            summary("a", "", "", 10, "Plan"),
            summary("d", "b", "a b", 50, "Re: Plan"),
        ];
        assert_eq!(order(&list), [(0, "".into()), (1, "└─".into())]);
    }

    #[test]
    fn loops_copies_and_messages_without_ids_stand_alone() {
        let list = [
            summary("a", "b", "b", 10, "A"),
            summary("b", "a", "a", 20, "B"),
            summary("a", "", "", 30, "A again"),
            summary("", "a", "a", 5, "No id"),
        ];
        // a answers b, so b can't answer a; the copy of a, newest, stands
        // alone; the message with no id still answers a.
        assert_eq!(
            order(&list),
            [
                (2, "".into()),
                (1, "".into()),
                (0, "└─".into()),
                (3, "  └─".into()),
            ]
        );
    }

    #[test]
    fn deep_conversations_stay_on_the_screen() {
        let list: Vec<Summary> = (0..50)
            .map(|i| {
                let parent = if i == 0 {
                    String::new()
                } else {
                    format!("m{}", i - 1)
                };
                summary(&format!("m{i}"), &parent, &parent, i, "Long")
            })
            .collect();
        let placed = order(&list);
        assert_eq!(placed.len(), 50);
        assert!(
            placed
                .iter()
                .all(|(_, t)| t.chars().count() <= 2 * MAX_DEPTH)
        );
    }

    #[test]
    fn a_full_list_is_threaded_at_once() {
        // Every message answers the one before, with a hundred
        // references: as many as a folder lists.
        let list: Vec<Summary> = (0..20_000)
            .map(|i| {
                let refs: Vec<String> = (i.max(100) - 100..i).map(|j| format!("m{j}")).collect();
                let parent = refs.last().cloned().unwrap_or_default();
                summary(&format!("m{i}"), &parent, &refs.join(" "), i, "Long")
            })
            .collect();
        let started = std::time::Instant::now();
        let placed = order(&list);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(placed.len(), 20_000);
        let roots = placed.iter().filter(|(_, tree)| tree.is_empty()).count();
        assert!(
            roots >= 20_000 / (MAX_CHAIN + 1),
            "a new one every thousand: {roots}"
        );
    }

    #[test]
    fn sent_messages_follow_connections_not_subjects_or_empty_ids() {
        let list = [
            summary("received", "sent", "missing sent", 30, "Plan"),
            summary("sent-again", "sent", "missing sent", 40, "Re: Plan"),
            summary("sent", "", "", 20, "Plan"),
            summary("sibling", "missing", "missing", 10, "Re: Plan"),
            summary("unrelated", "", "", 50, "Plan"),
            summary("", "", "", 60, "Plan"),
            summary("loop-a", "loop-b", "", 70, "Plan"),
            summary("loop-b", "loop-a", "", 80, "Plan"),
        ];
        let refs: Vec<&Summary> = list.iter().collect();
        assert_eq!(
            connected(&refs, 1),
            [true, true, true, true, false, false, false, false]
        );
        assert_eq!(connected(&refs, 0), [false; 8]);
    }

    #[test]
    fn a_reply_with_the_same_subject_needs_none() {
        let list = [
            summary("a", "", "", 10, "Plan"),
            summary("b", "a", "a", 20, "RE: Fwd:  plan"),
            summary("c", "a", "a", 30, "Re: Change of plan"),
        ];
        let refs: Vec<&Summary> = list.iter().collect();
        let same: Vec<bool> = arrange(&refs).iter().map(|p| p.same_subject).collect();
        assert_eq!(same, [false, true, false]);
    }
}
