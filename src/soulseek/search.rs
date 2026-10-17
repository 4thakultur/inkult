//! Network searches, searches in one user's shares, and the wishlist.
//!
//! Results arrive over minutes as individual peers answer, so a search is a
//! growing list of replies that the UI fetches incrementally. Everything a
//! search keeps is bounded: peers decide how much they send, not us.
//!
//! Wishlist terms are searched again periodically, at the interval the
//! server sets (usually 12 minutes, the network's own anti-flood rule).
//! Each term keeps one result list in which a file only appears once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde::Serialize;

use super::messages::{SearchReply, ServerRequest, SharedFile};
use super::session::Session;
use super::{MAX_QUERY_CHARS, SoulseekError, check_username, now_ms};
use crate::jobs::lock;
use crate::metadata::clean_text;

/// Searches kept per session; the oldest is dropped beyond this.
const MAX_SEARCHES: usize = 30;
const MAX_REPLIES: usize = 1500;
const MAX_FILES: usize = 25_000;
/// Files kept from one peer's reply.
const MAX_FILES_PER_REPLY: usize = 500;
const MAX_WISHLIST: usize = 50;
/// How long a search shows as running; replies still count afterwards.
const ACTIVE_FOR_MS: u64 = 45_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchScope {
    Network,
    User { username: String },
    Wishlist,
}

/// One peer's answer, as shown to the user.
#[derive(Debug, Clone, Serialize)]
pub struct Reply {
    pub username: String,
    pub slot_free: bool,
    /// Average upload speed in bytes per second, as the peer reports it.
    pub avg_speed: u32,
    pub queue_length: u32,
    pub files: Vec<SharedFile>,
    pub received_at_ms: u64,
}

struct Search {
    id: u64,
    query: String,
    scope: SearchScope,
    created_at_ms: u64,
    last_run_ms: u64,
    replies: Vec<Reply>,
    files: usize,
    stopped: bool,
    /// (user, path) already listed, for wishlist de-duplication.
    seen: HashSet<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchSummary {
    pub id: u64,
    pub query: String,
    pub scope: SearchScope,
    pub created_at_ms: u64,
    pub last_run_ms: u64,
    pub replies: usize,
    pub files: usize,
    pub active: bool,
    pub stopped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchView {
    #[serde(flatten)]
    pub summary: SearchSummary,
    /// Index of the first reply in `results` (for incremental fetching).
    pub from: usize,
    pub results: Vec<Reply>,
}

#[derive(Default)]
pub(super) struct Searches {
    list: VecDeque<Search>,
    tokens: HashMap<u32, u64>,
    next_id: u64,
    wishlist: Vec<String>,
    wishlist_next: usize,
}

impl Search {
    fn summary(&self, now: u64) -> SearchSummary {
        SearchSummary {
            id: self.id,
            query: self.query.clone(),
            scope: self.scope.clone(),
            created_at_ms: self.created_at_ms,
            last_run_ms: self.last_run_ms,
            replies: self.replies.len(),
            files: self.files,
            active: !self.stopped && now.saturating_sub(self.last_run_ms) < ACTIVE_FOR_MS,
            stopped: self.stopped,
        }
    }
}

impl Searches {
    fn create(&mut self, query: String, scope: SearchScope, token: u32) -> SearchSummary {
        self.next_id += 1;
        let now = now_ms();
        let search = Search {
            id: self.next_id,
            query,
            scope,
            created_at_ms: now,
            last_run_ms: now,
            replies: Vec::new(),
            files: 0,
            stopped: false,
            seen: HashSet::new(),
        };
        let summary = search.summary(now);
        self.tokens.insert(token, search.id);
        self.list.push_back(search);
        while self.list.len() > MAX_SEARCHES {
            if let Some(old) = self.list.pop_front() {
                self.tokens.retain(|_, id| *id != old.id);
            }
        }
        summary
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Search> {
        self.list.iter_mut().find(|s| s.id == id)
    }

    /// A reply from `username` (the connection's user, not the name the
    /// message claims).
    pub fn add_reply(&mut self, username: &str, reply: SearchReply) {
        let Some(&id) = self.tokens.get(&reply.token) else {
            return;
        };
        let Some(search) = self.get_mut(id) else {
            return;
        };
        if search.stopped || search.replies.len() >= MAX_REPLIES || search.files >= MAX_FILES {
            return;
        }
        let room = MAX_FILES - search.files;
        let wishlist = search.scope == SearchScope::Wishlist;
        let files: Vec<SharedFile> = reply
            .files
            .into_iter()
            .filter(|f| !f.name.is_empty())
            .filter(|f| !wishlist || search.seen.insert((username.to_owned(), f.name.clone())))
            .take(MAX_FILES_PER_REPLY.min(room))
            .collect();
        if files.is_empty() {
            return;
        }
        search.files += files.len();
        search.replies.push(Reply {
            username: username.to_owned(),
            slot_free: reply.slot_free,
            avg_speed: reply.avg_speed,
            queue_length: reply.queue_length,
            files,
            received_at_ms: now_ms(),
        });
    }

    pub fn summaries(&self) -> Vec<SearchSummary> {
        let now = now_ms();
        self.list.iter().rev().map(|s| s.summary(now)).collect()
    }

    pub fn view(&self, id: u64, from: usize) -> Option<SearchView> {
        let search = self.list.iter().find(|s| s.id == id)?;
        let from = from.min(search.replies.len());
        Some(SearchView {
            summary: search.summary(now_ms()),
            from,
            results: search.replies[from..].to_vec(),
        })
    }

    pub fn stop(&mut self, id: u64) -> bool {
        match self.get_mut(id) {
            Some(search) => {
                search.stopped = true;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.list.len();
        self.list.retain(|s| s.id != id);
        self.tokens.retain(|_, sid| *sid != id);
        before != self.list.len()
    }

    pub fn set_wishlist(&mut self, terms: &[String]) {
        self.wishlist = terms.to_vec();
        self.wishlist_next = 0;
    }

    /// The next wishlist term to search, round robin.
    fn next_wishlist_term(&mut self) -> Option<String> {
        if self.wishlist.is_empty() {
            return None;
        }
        let term = self.wishlist[self.wishlist_next % self.wishlist.len()].clone();
        self.wishlist_next = (self.wishlist_next + 1) % self.wishlist.len();
        Some(term)
    }

    /// Route a new run of a wishlist term to its result list.
    fn wishlist_run(&mut self, term: String, token: u32) {
        let existing = self
            .list
            .iter_mut()
            .find(|s| s.scope == SearchScope::Wishlist && s.query == term);
        match existing {
            Some(search) => {
                search.last_run_ms = now_ms();
                search.stopped = false;
                let id = search.id;
                self.tokens.insert(token, id);
            }
            None => {
                self.create(term, SearchScope::Wishlist, token);
            }
        }
    }
}

fn clean_query(raw: &str) -> Result<String, SoulseekError> {
    clean_text(raw)
        .filter(|q| q.chars().count() <= MAX_QUERY_CHARS)
        .ok_or_else(|| {
            SoulseekError::InvalidInput(format!(
                "search terms must be 1–{MAX_QUERY_CHARS} characters"
            ))
        })
}

impl Session {
    /// Search the whole network, or only `user`'s shares.
    pub fn search(&self, query: &str, user: Option<&str>) -> Result<SearchSummary, SoulseekError> {
        let query = clean_query(query)?;
        let user = user.map(check_username).transpose()?;
        if !self.is_online() {
            return Err(SoulseekError::Offline);
        }
        let token = self.next_token();
        let (scope, request) = match user {
            Some(username) => (
                SearchScope::User {
                    username: username.clone(),
                },
                ServerRequest::UserSearch {
                    username,
                    token,
                    query: query.clone(),
                },
            ),
            None => (
                SearchScope::Network,
                ServerRequest::FileSearch {
                    token,
                    query: query.clone(),
                },
            ),
        };
        let summary = lock(&self.searches).create(query, scope, token);
        self.send_server(request)?;
        tracing::info!(owner = %self.owner, search = summary.id, "Soulseek search started");
        Ok(summary)
    }

    pub fn searches(&self) -> Vec<SearchSummary> {
        lock(&self.searches).summaries()
    }

    pub fn search_results(&self, id: u64, from: usize) -> Option<SearchView> {
        lock(&self.searches).view(id, from)
    }

    pub fn stop_search(&self, id: u64) -> bool {
        lock(&self.searches).stop(id)
    }

    pub fn remove_search(&self, id: u64) -> bool {
        lock(&self.searches).remove(id)
    }

    /// Add or remove a wishlist term; returns the new list.
    pub async fn edit_wishlist(&self, term: &str, add: bool) -> Result<Vec<String>, SoulseekError> {
        let term = clean_query(term)?;
        let list = self
            .ctx
            .store
            .update(self.owner, |account| {
                if add {
                    if account.wishlist.len() >= MAX_WISHLIST {
                        return Err(SoulseekError::Limit(format!(
                            "the wishlist holds at most {MAX_WISHLIST} terms"
                        )));
                    }
                    if !account
                        .wishlist
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(&term))
                    {
                        account.wishlist.push(term.clone());
                    }
                } else {
                    account.wishlist.retain(|t| !t.eq_ignore_ascii_case(&term));
                }
                Ok(account.wishlist.clone())
            })
            .await?;
        lock(&self.searches).set_wishlist(&list);
        if add && self.is_online() {
            // Search right away; later runs follow the server's interval.
            let token = self.next_token();
            lock(&self.searches).wishlist_run(term.clone(), token);
            self.send_server(ServerRequest::WishlistSearch { token, query: term })?;
        }
        Ok(list)
    }

    pub(super) async fn wishlist_loop(self: Arc<Self>) {
        loop {
            let interval = self.wishlist_interval();
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.shutdown.cancelled() => return,
            }
            if !self.is_online() {
                continue;
            }
            let token = self.next_token();
            let term = {
                let mut searches = lock(&self.searches);
                let term = searches.next_wishlist_term();
                if let Some(term) = &term {
                    searches.wishlist_run(term.clone(), token);
                }
                term
            };
            if let Some(query) = term {
                let _ = self.send_server(ServerRequest::WishlistSearch { token, query });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soulseek::messages::FileAttrs;

    fn reply(token: u32, names: &[&str]) -> SearchReply {
        SearchReply {
            username: "liar".into(),
            token,
            files: names
                .iter()
                .map(|n| SharedFile {
                    name: (*n).into(),
                    size: 1,
                    attrs: FileAttrs::default(),
                })
                .collect(),
            slot_free: true,
            avg_speed: 1,
            queue_length: 0,
        }
    }

    #[test]
    fn collects_replies_by_token_under_the_real_username() {
        let mut searches = Searches::default();
        let s = searches.create("glue".into(), SearchScope::Network, 10);
        searches.add_reply("alice", reply(10, &["a\\glue.flac"]));
        searches.add_reply("bob", reply(99, &["b\\glue.flac"])); // unknown token
        let view = searches.view(s.id, 0).unwrap();
        assert_eq!(view.results.len(), 1);
        assert_eq!(view.results[0].username, "alice");
        assert_eq!(searches.view(s.id, 1).unwrap().results.len(), 0);

        searches.stop(s.id);
        searches.add_reply("carol", reply(10, &["c\\glue.flac"]));
        assert_eq!(searches.view(s.id, 0).unwrap().summary.replies, 1);
    }

    #[test]
    fn wishlist_runs_share_one_deduplicated_list() {
        let mut searches = Searches::default();
        searches.set_wishlist(&["rare dub".into(), "white label".into()]);
        assert_eq!(searches.next_wishlist_term().as_deref(), Some("rare dub"));
        assert_eq!(
            searches.next_wishlist_term().as_deref(),
            Some("white label")
        );
        searches.wishlist_run("rare dub".into(), 1);
        searches.add_reply("alice", reply(1, &["a\\dub.flac"]));
        searches.wishlist_run("rare dub".into(), 2);
        searches.add_reply("alice", reply(2, &["a\\dub.flac", "a\\dub2.flac"]));
        let summaries = searches.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].files, 2);
    }

    #[test]
    fn bounds_searches_and_files() {
        let mut searches = Searches::default();
        for token in 0..(MAX_SEARCHES as u32 + 5) {
            searches.create(format!("q{token}"), SearchScope::Network, token);
        }
        assert_eq!(searches.summaries().len(), MAX_SEARCHES);
        assert!(!searches.tokens.contains_key(&0));

        let names: Vec<String> = (0..2000).map(|i| format!("x\\{i}.mp3")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let s = searches.create("big".into(), SearchScope::Network, 1000);
        searches.add_reply("flood", reply(1000, &refs));
        assert_eq!(
            searches.view(s.id, 0).unwrap().summary.files,
            MAX_FILES_PER_REPLY
        );
    }
}
