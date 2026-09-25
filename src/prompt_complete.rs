//! Prompt-line (prose) completion — the host-side layer between the
//! line editor and the FS completer (doc: `doc/slashdot/
//! arg_and_modifier_autocomplete.md` §Prompt-line completion).
//!
//! One instance per prompt session; spawns a worker thread so FS IO
//! (`path_complete::FileCompleter`) never blocks the input loop.
//! [`PromptCompletion::eval`] drives one frame:
//!
//! 1. word under the cursor (char space, [`completable`] gate),
//! 2. drain word-tagged results — a result displays only while it
//!    still matches the word under the cursor,
//! 3. arm/debounce a request for a changed word (100 ms stable),
//! 4. render: [`Frame::Idle`] (no dialog) or [`Frame::Dialog`].
//!
//! On `Tab` the selected row's id is spliced over the word range in
//! place (the cursor lands after the id) and its re-run is kicked
//! immediately; an empty re-run appends the trailing-space
//! convenience. `Enter`/`Esc` never reach this layer — the surrounding
//! input loop keeps their semantics (submit / no-op).

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::path_complete::{completable, listing_stale, FileCompleter};
use crate::repl::LineEditor;
use crate::slashdot_menu::fields::ColumnDef;
use crate::slashdot_menu::node_impl::{CompletionRows, RenderContext};
use crate::slashdot_menu::render::RenderPlan;
use crate::slashdot_menu::view::{RowView, ViewRequest, ViewRows};
use crate::slashdot_menu::HostKey;

/// A new word must hold still this long before its request is sent —
/// keeps burst typing at string cost (one listing after the pause).
const DEBOUNCE: Duration = Duration::from_millis(100);

/// Wake interval while a request is in flight, so a result is drained
/// shortly after the worker sends it (no key needed).
const RESULT_SLICE: Duration = Duration::from_millis(20);

/// What one eval frame needs from the input loop. Splicing (Tab)
/// mutates the editor in place — no buffer or cursor crosses this
/// boundary.
pub enum Frame {
    /// No dialog: inert/no word, request pending, or bypass key.
    Idle,
    /// Paint this plan; the buffer is unchanged.
    Dialog(RenderPlan),
}

/// Word directly before the char-index `cursor` (char range into
/// `buf`); `None` at the buffer start, after whitespace, or when the
/// cursor sits *inside* a token (mid-word ⇒ inert — completion never
/// opens on a word the user is still editing).
///
/// Delimiter = whitespace (` ` / `\n` / `\t` — any `char::is_whitespace`),
/// which scopes the word to the cursor's logical line for free.
pub fn word_at_cursor(buf: &[char], cursor: usize) -> Option<Range<usize>> {
    let n = buf.len();
    let cursor = cursor.min(n);
    if cursor == 0 {
        return None;
    }
    // After whitespace ⇒ no word directly before the cursor (a space
    // dismisses — same rule as `PathArgs::word_before`).
    if buf[cursor - 1].is_whitespace() {
        return None;
    }
    // Token continues past the cursor ⇒ mid-word ⇒ inert.
    if cursor < n && !buf[cursor].is_whitespace() {
        return None;
    }
    let mut start = cursor;
    while start > 0 && !buf[start - 1].is_whitespace() {
        start -= 1;
    }
    Some(start..cursor)
}

/// Background worker: owns the `FileCompleter` (and thus the FS IO).
/// Coalesces a burst to its newest request before computing, so a
/// typing race costs at most one wasted listing.
fn worker_loop(rx_req: Receiver<String>, tx_res: Sender<(String, CompletionRows)>) {
    let mut completer = FileCompleter::new();
    while let Ok(first) = rx_req.recv() {
        let mut word = first;
        while let Ok(w) = rx_req.try_recv() {
            word = w;
        }
        let rows = completer.complete(&word);
        if tx_res.send((word, rows)).is_err() {
            break;
        }
    }
}

pub struct PromptCompletion {
    /// Dialog cursor + row-line cache (engine-owned, like slash).
    view: RowView,
    /// Word → rows → view: the wrap memo. Keeps the same
    /// `Rc<ViewRows>` for an unchanged word, so `plan` skips the
    /// cursor reconcile across passive frames.
    last: Option<(String, Rc<CompletionRows>, Rc<ViewRows>)>,
    tx: Sender<String>,
    rx: Receiver<(String, CompletionRows)>,
    /// Word whose request is in flight (dedupe: never re-sent).
    outstanding: Option<String>,
    /// Debounce: (word, stable-since) waiting out [`DEBOUNCE`].
    armed: Option<(String, Instant)>,
    /// Tab-expanded id awaiting its re-run result: an empty re-run
    /// appends the trailing space. Cleared when the word moves on.
    expand: Option<String>,
    /// Worker died (`rx` disconnected): stop sending / polling.
    dead: bool,
}

impl PromptCompletion {
    /// Spawn the completion worker for one prompt session. The worker
    /// exits when this (and its `Sender`) drops.
    pub fn new() -> Self {
        let (tx, rx_req) = channel::<String>();
        let (tx_res, rx) = channel::<(String, CompletionRows)>();
        std::thread::spawn(move || worker_loop(rx_req, tx_res));
        Self {
            view: RowView::new(),
            last: None,
            tx,
            rx,
            outstanding: None,
            armed: None,
            expand: None,
            dead: false,
        }
    }

    /// How long the input loop may sleep before re-evaluating:
    /// `None` = block on `event::read()`; `Some(d)` = `event::poll(d)`
    /// and re-eval on timeout (debounce deadline and/or a slice
    /// waiting on an in-flight result).
    pub fn poll_timeout(&self) -> Option<Duration> {
        if self.dead {
            return None;
        }
        let now = Instant::now();
        let debounce = self
            .armed
            .as_ref()
            .map(|(_, t)| (*t + DEBOUNCE).saturating_duration_since(now));
        let slice = if self.outstanding.is_some() {
            Some(RESULT_SLICE)
        } else {
            None
        };
        match (debounce, slice) {
            (None, None) => None,
            (Some(d), None) => Some(d),
            (None, Some(s)) => Some(s),
            (Some(d), Some(s)) => Some(d.min(s)),
        }
    }

    /// One passive / `MoveCursor` / `Tab` frame. On `Tab` the selected
    /// id is spliced over the word range in place (`LineEditor::
    /// replace_range`, char range — the cursor lands after the id) and
    /// its re-run is kicked immediately; the caller re-derives on the
    /// next frame. `Enter`/`Esc` never reach here.
    pub fn eval(
        &mut self,
        editor: &mut LineEditor,
        key: HostKey,
        ctx: RenderContext,
    ) -> Frame {
        // 1. Word under the cursor (char space, gated).
        let mut word: Option<(Range<usize>, String)> =
            word_at_cursor(editor.chars(), editor.cursor()).and_then(|r| {
                let w: String = editor.chars()[r.clone()].iter().collect();
                completable(&w).then(|| (r, w))
            });

        // 2. A pending Tab-expansion counts only while the word under
        //    the cursor is still exactly the expanded id.
        if let Some(exp) = &self.expand {
            if word.as_ref().map(|(_, w)| w) != Some(exp) {
                self.expand = None;
            }
        }

        // 3. Drain results (word-tagged; only the current word shows).
        let mut expanded_final = false;
        loop {
            match self.rx.try_recv() {
                Ok((w, rows)) => {
                    if self.outstanding.as_deref() == Some(w.as_str()) {
                        self.outstanding = None;
                    }
                    let is_current = word.as_ref().map(|(_, cw)| cw == &w).unwrap_or(false);
                    if !is_current {
                        continue; // stale word — dropped, never displayed
                    }
                    let empty = rows.rows.is_empty();
                    if self.expand.as_deref() == Some(w.as_str()) {
                        self.expand = None;
                        if empty {
                            // Re-run empty ⇒ trailing-space convenience,
                            // unless we're mid-text with a space already
                            // following the cursor — then just consume it.
                            if editor.cursor() < editor.chars().len()
                                && editor.chars()[editor.cursor()] == ' '
                            {
                                editor.right();
                            } else {
                                editor.insert(' ');
                            }
                            expanded_final = true;
                        }
                    }
                    if empty {
                        // Exact / nothing left to complete: no dialog.
                        self.last = Some((w, Rc::new(rows), ViewRows::of(Vec::new())));
                        self.view.reset();
                    } else {
                        let rc = Rc::new(rows);
                        let vrows = if let Some(ref match_indices) = rc.match_indices {
                            // Convert from row index to RowRef key
                            let mut map = HashMap::new();
                            for (idx, indices) in match_indices {
                                let rref = crate::slashdot_menu::RowRef {
                                    child_idx: 0,
                                    row_offset: *idx as u32,
                                };
                                map.insert(rref, indices.clone());
                            }
                            ViewRows::of_with_match_indices(rc.rows.clone(), map)
                        } else {
                            ViewRows::of(rc.rows.clone())
                        };
                        self.last = Some((w, rc, vrows));
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.dead = true;
                    self.outstanding = None;
                    self.armed = None;
                    break;
                }
            }
        }
        if expanded_final {
            // The trailing space dismisses the word (step 1 rule on the
            // next frame); this frame has nothing to complete.
            word = None;
        }

        // 4. Arm / send (debounced; the same word is never re-sent
        //    while a result is cached or in flight).
        if self.dead {
            self.armed = None;
        } else {
            match &word {
                None => self.armed = None,
                Some((_, w)) => {
                    let have = self
                        .last
                        .as_ref()
                        .map(|(lw, _, _)| lw == w)
                        .unwrap_or(false)
                        && !listing_stale(true);
                    if have || self.outstanding.as_deref() == Some(w.as_str()) {
                        self.armed = None;
                    } else {
                        let ready = match &self.armed {
                            Some((aw, t)) => aw == w && t.elapsed() >= DEBOUNCE,
                            None => false,
                        };
                        if ready {
                            self.send(w.clone());
                            self.armed = None;
                        } else if !matches!(&self.armed, Some((aw, _)) if aw == w) {
                            // First frame with this word (or it changed).
                            self.armed = Some((w.clone(), Instant::now()));
                        }
                    }
                }
            }
        }

        // 5. Display / Tab. The dialog shows rows only while they
        //    belong to the word under the cursor (pending ⇒ Idle).
        let show = word.as_ref().and_then(|(r, w)| {
            self.last.as_ref().and_then(|(lw, rc, vrows)| {
                (lw == w && !rc.rows.is_empty())
                    .then(|| (r.clone(), rc.labels.clone(), Rc::clone(vrows)))
            })
        });
        match key {
            HostKey::Tab => {
                let Some((range, labels, vrows)) = show else {
                    return Frame::Idle; // no dialog ⇒ Tab inert
                };
                let columns = Self::columns_for(labels);
                let out = self.view.plan(
                    vrows,
                    &ViewRequest {
                        columns: &columns,
                        detail: Vec::new(),
                        seed_id: None,
                        pin: None,
                        cursor_delta: None,
                        render_ctx: ctx,
                    },
                );
                let Some(row) = out.cursor_row else {
                    return Frame::Idle;
                };
                // In-place expansion: replace the word range with the
                // row id (cursor lands after it), then re-run at once —
                // no debounce on an explicit user action.
                editor.replace_range(range.start, range.end, &row.id);
                self.expand = Some(row.id.clone());
                self.armed = None;
                self.send(row.id);
                Frame::Idle
            }
            HostKey::MoveCursor(d) => {
                let Some((_r, labels, vrows)) = show else {
                    return Frame::Idle;
                };
                let columns = Self::columns_for(labels);
                let out = self.view.plan(
                    vrows,
                    &ViewRequest {
                        columns: &columns,
                        detail: Vec::new(),
                        seed_id: None,
                        pin: None,
                        cursor_delta: Some(d),
                        render_ctx: ctx,
                    },
                );
                Frame::Dialog(out.plan)
            }
            _ => {
                // Passive frame: re-derive the dialog (or dismiss).
                if let Some((_r, labels, vrows)) = show {
                    let columns = Self::columns_for(labels);
                    let out = self.view.plan(
                        vrows,
                        &ViewRequest {
                            columns: &columns,
                            detail: Vec::new(),
                            seed_id: None,
                            pin: None,
                            cursor_delta: None,
                            render_ctx: ctx,
                        },
                    );
                    Frame::Dialog(out.plan)
                } else {
                    if word.is_none() {
                        // Dismissed (space / non-completable word): a
                        // reopen starts at row 0.
                        self.view.reset();
                    }
                    Frame::Idle
                }
            }
        }
    }

    /// Dialog columns from the completer's labels (field names stay
    /// `id`/`description`; only the headers differ). Fallback pair for
    /// a label-less response — the render path is label-agnostic.
    fn columns_for(labels: Option<(String, String)>) -> [ColumnDef; 2] {
        let (l1, l2) = labels.unwrap_or_else(|| ("Path".into(), "Description".into()));
        [ColumnDef::new("id", l1), ColumnDef::new("description", l2)]
    }

    fn send(&mut self, word: String) {
        if self.dead {
            return;
        }
        if self.tx.send(word.clone()).is_ok() {
            self.outstanding = Some(word);
        } else {
            self.dead = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::fields::Row;

    /// Harness without a worker: the test holds the request receiver
    /// (what the worker would see) and the result sender (what the
    /// worker would push).
    fn harness() -> (PromptCompletion, Receiver<String>, Sender<(String, CompletionRows)>) {
        let (tx, rx_req) = channel();
        let (tx_res, rx) = channel();
        (
            PromptCompletion {
                view: RowView::new(),
                last: None,
                tx,
                rx,
                outstanding: None,
                armed: None,
                expand: None,
                dead: false,
            },
            rx_req,
            tx_res,
        )
    }

    fn ctx() -> RenderContext {
        RenderContext { term_width: 80 }
    }

    fn rows(ids: &[&str]) -> CompletionRows {
        CompletionRows {
            rows: ids.iter().map(|i| Row::new(*i)).collect(),
            labels: Some(("Path".into(), "Description".into())),
            match_indices: None,
        }
    }

    fn editor_at(text: &str, cursor: usize) -> LineEditor {
        let mut e = LineEditor::new();
        e.set(text);
        while e.cursor() > cursor {
            e.left();
        }
        e
    }

    fn selected(frame: &Frame) -> Option<usize> {
        match frame {
            Frame::Dialog(p) => p.selected,
            Frame::Idle => panic!("expected a dialog"),
        }
    }

    fn idle(frame: &Frame) {
        assert!(matches!(frame, Frame::Idle), "expected Idle");
    }

    // --- word_at_cursor ---

    #[test]
    fn word_at_cursor_boundaries() {
        let w: Vec<char> = "hello world".chars().collect();
        assert_eq!(word_at_cursor(&[], 0), None);
        assert_eq!(word_at_cursor(&w, 0), None, "buffer start");
        assert_eq!(word_at_cursor(&w, 5), Some(0..5), "word ends at cursor");
        assert_eq!(word_at_cursor(&w, 6), None, "after space");
        assert_eq!(word_at_cursor(&w, 7), None, "mid-word");
        assert_eq!(word_at_cursor(&w, 11), Some(6..11), "EOL");
        // Mid-line: word ends exactly at the cursor, text follows.
        let m: Vec<char> = "cat ./src more".chars().collect();
        assert_eq!(word_at_cursor(&m, 9), Some(4..9));
        assert_eq!(word_at_cursor(&m, 6), None, "mid-word stays inert");
    }

    #[test]
    fn word_at_cursor_scopes_to_logical_line() {
        let b: Vec<char> = "one\ntwo\tthree".chars().collect();
        // Cursor at start of line 2 (right after '\n').
        assert_eq!(word_at_cursor(&b, 4), None, "after newline");
        assert_eq!(word_at_cursor(&b, 7), Some(4..7), "word on line 2");
        assert_eq!(word_at_cursor(&b, 13), Some(8..13), "tab-delimited");
        assert_eq!(word_at_cursor(&b, 14), Some(8..13), "past len clamps");
    }

    // --- request lifecycle: gate / debounce / dedupe ---

    #[test]
    fn bare_word_never_armed() {
        let (mut pc, req, _res) = harness();
        let mut ed = editor_at("cat fo", 6);
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert!(pc.armed.is_none(), "gate miss must not arm");
        assert!(req.try_recv().is_err(), "no request for bare words");
        assert!(pc.poll_timeout().is_none(), "nothing pending ⇒ block");
    }

    #[test]
    fn completable_word_debounces_then_sends_once() {
        let (mut pc, req, _res) = harness();
        let mut ed = editor_at("cat ./src", 9);
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        // Armed, not sent: debounce still running.
        assert!(pc.armed.is_some());
        assert!(req.try_recv().is_err());
        assert!(pc.poll_timeout().unwrap() <= DEBOUNCE, "wake at deadline");

        // Hold still past the deadline → sent, outstanding set.
        pc.armed = Some(("./src".into(), Instant::now() - DEBOUNCE));
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(req.try_recv().unwrap(), "./src");
        assert_eq!(pc.outstanding.as_deref(), Some("./src"));
        assert!(pc.armed.is_none());
        assert!(pc.poll_timeout().unwrap() <= RESULT_SLICE, "poll for result");

        // Same word again: never re-sent (dedupe on outstanding).
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert!(req.try_recv().is_err(), "no duplicate request");
    }

    #[test]
    fn word_change_rearms_and_supersedes() {
        let (mut pc, req, _res) = harness();
        let mut ed = editor_at("cat ./a", 7);
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        pc.armed = Some(("./a".into(), Instant::now() - DEBOUNCE));
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(req.try_recv().unwrap(), "./a");

        // Typing on: './ab' supersedes — debounced, then sent while
        // './a' may still be in flight.
        ed.insert('b');
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert!(pc.armed.as_ref().unwrap().0 == "./ab", "rearmed on change");
        pc.armed = Some(("./ab".into(), Instant::now() - DEBOUNCE));
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(req.try_recv().unwrap(), "./ab");
        assert_eq!(pc.outstanding.as_deref(), Some("./ab"));

        // Stale result for './a': dropped, does not clear './ab'.
        _res.send(("./a".into(), rows(&["./a1", "./a2"]))).unwrap();
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(pc.outstanding.as_deref(), Some("./ab"));
        assert!(pc.last.is_none() || pc.last.as_ref().unwrap().0 != "./a");
    }

    // --- display: word-match rule ---

    #[test]
    fn matching_result_displays_and_tracks_cursor() {
        let (mut pc, req, res) = harness();
        let mut ed = editor_at("cat ./src", 9);
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        pc.armed = Some(("./src".into(), Instant::now() - DEBOUNCE));
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(req.try_recv().unwrap(), "./src");

        res.send(("./src".into(), rows(&["./src/a", "./src/b", "./src/c"])))
            .unwrap();
        let f = pc.eval(&mut ed, HostKey::None, ctx());
        assert_eq!(selected(&f), Some(0), "seeds row 0");

        let f = pc.eval(&mut ed, HostKey::MoveCursor(1), ctx());
        assert_eq!(selected(&f), Some(1), "delta applied");

        // Re-deriving with a fresh Rc for the same word keeps the
        // moved cursor (reconcile keeps the id).
        res.send(("./src".into(), rows(&["./src/a", "./src/b", "./src/c"])))
            .unwrap();
        let f = pc.eval(&mut ed, HostKey::None, ctx());
        assert_eq!(selected(&f), Some(1), "cursor survives re-derive");
    }

    #[test]
    fn word_change_pending_is_idle_never_stale_rows() {
        let (mut pc, _req, res) = harness();
        let mut ed = editor_at("cat ./a", 7);
        res.send(("./a".into(), rows(&["./a1", "./a2"]))).unwrap();
        let f = pc.eval(&mut ed, HostKey::None, ctx());
        selected(&f); // shown for './a'

        // Typing changes the word: stale rows must not display.
        ed.insert('b');
        let f = pc.eval(&mut ed, HostKey::None, ctx());
        idle(&f);
    }

    #[test]
    fn empty_result_dismisses_and_resets_cursor() {
        let (mut pc, _req, res) = harness();
        let mut ed = editor_at("cat ./only", 10);
        res.send(("./only".into(), rows(&["./a", "./b", "./c"]))).unwrap();
        assert_eq!(selected(&pc.eval(&mut ed, HostKey::None, ctx())), Some(0));
        assert_eq!(selected(&pc.eval(&mut ed, HostKey::MoveCursor(1), ctx())), Some(1));

        // Re-run comes back all-exact ⇒ empty ⇒ dismissed (Idle).
        res.send(("./only".into(), CompletionRows { rows: Vec::new(), labels: None, match_indices: None }))
            .unwrap();
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));

        // A later dialog for the same word reseeds at row 0 — the
        // reset cleared the moved cursor (without it, the kept id
        // would land back on row 1).
        res.send(("./only".into(), rows(&["./a", "./b", "./c"]))).unwrap();
        assert_eq!(selected(&pc.eval(&mut ed, HostKey::None, ctx())), Some(0));
    }

    // --- Tab expansion ---

    #[test]
    fn tab_splices_word_and_kicks_rerun() {
        let (mut pc, req, res) = harness();
        let mut ed = editor_at("cat ./sr", 8);
        res.send(("./sr".into(), rows(&["./src/", "./srv/"]))).unwrap();
        selected(&pc.eval(&mut ed, HostKey::None, ctx()));

        // Move to the second row, then Tab: splice in place.
        assert_eq!(selected(&pc.eval(&mut ed, HostKey::MoveCursor(1), ctx())), Some(1));
        let f = pc.eval(&mut ed, HostKey::Tab, ctx());
        idle(&f);
        assert_eq!(ed.text(), "cat ./srv/", "word replaced, prefix kept");
        assert_eq!(ed.cursor(), 10, "cursor lands after the id");
        assert_eq!(req.try_recv().unwrap(), "./srv/", "re-run kicked at once");
        assert_eq!(pc.expand.as_deref(), Some("./srv/"));
        assert!(pc.armed.is_none(), "no debounce on an explicit action");
    }

    #[test]
    fn tab_rerun_empty_appends_trailing_space() {
        let (mut pc, _req, res) = harness();
        let mut ed = editor_at("./fi", 4);
        res.send(("./fi".into(), rows(&["./file.txt"]))).unwrap();
        selected(&pc.eval(&mut ed, HostKey::None, ctx()));
        idle(&pc.eval(&mut ed, HostKey::Tab, ctx()));
        assert_eq!(ed.text(), "./file.txt");

        res.send(("./file.txt".into(), CompletionRows { rows: Vec::new(), labels: None, match_indices: None }))
            .unwrap();
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(ed.text(), "./file.txt ", "trailing space appended");
        assert_eq!(ed.cursor(), 11);
        // The space dismisses on the following frame.
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert!(pc.armed.is_none());
    }

    #[test]
    fn tab_rerun_nonempty_continues_without_space() {
        let (mut pc, _req, res) = harness();
        let mut ed = editor_at("./sr", 4);
        res.send(("./sr".into(), rows(&["./src/"]))).unwrap();
        selected(&pc.eval(&mut ed, HostKey::None, ctx()));
        idle(&pc.eval(&mut ed, HostKey::Tab, ctx()));
        assert_eq!(ed.text(), "./src/");

        res.send(("./src/".into(), rows(&["./src/a", "./src/b"]))).unwrap();
        let f = pc.eval(&mut ed, HostKey::None, ctx());
        selected(&f);
        assert_eq!(ed.text(), "./src/", "drill continues, no space");
    }

    #[test]
    fn tab_without_dialog_is_inert() {
        let (mut pc, req, _res) = harness();
        let mut ed = editor_at("hello", 5);
        idle(&pc.eval(&mut ed, HostKey::Tab, ctx()));
        assert_eq!(ed.text(), "hello", "no splice");
        assert!(req.try_recv().is_err(), "no request");
    }

    #[test]
    fn expansion_invalidated_by_edit_never_spaces() {
        let (mut pc, _req, res) = harness();
        let mut ed = editor_at("./fi", 4);
        res.send(("./fi".into(), rows(&["./file.txt"]))).unwrap();
        selected(&pc.eval(&mut ed, HostKey::None, ctx()));
        idle(&pc.eval(&mut ed, HostKey::Tab, ctx()));
        assert_eq!(ed.text(), "./file.txt");

        // User types on before the re-run lands: expansion void.
        ed.insert('x');
        res.send(("./file.txt".into(), CompletionRows { rows: Vec::new(), labels: None, match_indices: None }))
            .unwrap();
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert_eq!(ed.text(), "./file.txtx", "no space for a void expansion");
    }

    #[test]
    fn tab_rerun_empty_mid_text_consumes_existing_space() {
        let (mut pc, _req, res) = harness();
        // Cursor at end of "./fi" with a space already after it
        let mut ed = editor_at("./fi ", 4);
        res.send(("./fi".into(), rows(&["./file.txt"]))).unwrap();
        selected(&pc.eval(&mut ed, HostKey::None, ctx()));
        idle(&pc.eval(&mut ed, HostKey::Tab, ctx()));
        assert_eq!(ed.text(), "./file.txt ");

        res.send(("./file.txt".into(), CompletionRows { rows: Vec::new(), labels: None, match_indices: None }))
            .unwrap();
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        // Should consume the existing space, not add a second one
        assert_eq!(ed.text(), "./file.txt ", "single space preserved");
        assert_eq!(ed.cursor(), 11, "cursor advanced past consumed space");
    }

    // --- worker death ---

    #[test]
    fn dead_worker_stops_polling_and_sending() {
        let (mut pc, _req, res) = harness();
        drop(res); // worker "dies": result channel disconnects
        let mut ed = editor_at("./src", 5);
        idle(&pc.eval(&mut ed, HostKey::None, ctx()));
        assert!(pc.dead);
        assert!(pc.poll_timeout().is_none(), "no busy-poll on a dead worker");
        assert!(pc.armed.is_none());
    }
}
