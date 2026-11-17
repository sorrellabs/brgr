//! Keeps one caller's workers in a single area beside it, divided evenly.
//!
//! The first worker splits the caller: to the right of a wide caller, below a
//! narrow one. Later workers fill that area as rows of pairs: a new row under
//! the last when it still fits, then a second pane to the right of the first
//! pane in each row, top to bottom. True columns `(W1/W2/W3) | (W4/W5/W6)`
//! cannot be built: Herdr will not move a pane within its tab (`same_tab`),
//! and `pane split` only splits a leaf, so a full-width row cannot be added
//! under a pair. The caller is never split for a later worker. A narrow
//! caller's workers stay in one column: side by side they would be too narrow
//! for a TUI. When both columns are full, or the area is too narrow or too
//! short for another readable pane, the next worker opens in a new tab.
//! When a worker closes, its neighbour takes its space, so the rest are made
//! equal again.
//!
//! With `worker-placement = below`, the first worker splits the caller down
//! and later workers split the last (rightmost) worker of that row right, so
//! several heads can share a tab, each above its own row. A full row, or a
//! caller too narrow or too short to halve, opens the next worker in a new
//! tab; there is no second row. Resizes are only `right` on a worker that has
//! a row-mate to its right and `left` on a worker that has one to its left.
//! `up`, `down`, `right` on the last of a row, `left` on the first, and any
//! resize of the caller would move a head border, so they are never sent.
//!
//! Herdr's split tree is binary. Splitting the last worker down, then pairing
//! each row, makes the area a chain of rows, each row a pair. The `down` split
//! whose first child is a row holds the rest of the chain and gets ratio
//! `1 / (n - i)`; each pair is set to 1/2. `herdr pane resize --direction down`
//! on a pane raises the ratio of the split below it; `up` on the pane under
//! that border lowers it; `right` on the left pane of a pair raises the pair's
//! ratio; `left` on the right pane lowers it. `right` is never sent for a pane
//! with no worker to its right, and `left` never for a pane with none to its
//! left: those would move the caller's border. A negative amount is not a
//! decrease, so it is never sent.

use std::{fs, path::Path};

use serde_json::Value;

use super::lifecycle::PaneReceipt;

/// A caller pane narrower than this many columns gets its workers below it.
pub(super) const NARROW_CALLER: u64 = 100;
/// Below this many rows a stacked worker cannot show a startup dialog whole.
/// Claude Code's folder-trust dialog needs about 17 rows; shorter, its
/// `Accessing workspace:` line and path scroll off, brgr cannot prove the
/// dialog is for the task's workspace, and the worker waits on it.
const MIN_ROWS: u64 = 20;
/// A worker pane narrower than this cannot hold a TUI. Half of `NARROW_CALLER`:
/// the narrowest worker a right split of a wide caller already produces.
const MIN_COLS: u64 = NARROW_CALLER / 2; // 50
/// Rows of an area wide enough for two columns. Claude Code's trust dialog is
/// 22 rows at 53 columns with a path that wraps to four rows.
const MIN_ROWS_PAIRED: u64 = 24;
/// Ratio differences smaller than this are rounding, not imbalance.
const TOLERANCE: f64 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
}

impl Rect {
    fn from(value: &Value) -> Option<Self> {
        Some(Self {
            x: value.get("x").and_then(Value::as_u64).unwrap_or(0),
            y: value.get("y").and_then(Value::as_u64).unwrap_or(0),
            width: value.get("width")?.as_u64()?,
            height: value.get("height")?.as_u64()?,
        })
    }
}

#[derive(Debug)]
struct Split {
    down: bool,
    ratio: f64,
    rect: Rect,
}

/// One tab's panes and splits, as `herdr pane layout` reports them.
#[derive(Debug)]
pub(super) struct Layout {
    panes: Vec<(String, Rect)>,
    splits: Vec<Split>,
}

impl Layout {
    pub(super) fn parse(response: &Value) -> Option<Self> {
        let layout = response.pointer("/result/layout")?;
        let panes = layout
            .get("panes")?
            .as_array()?
            .iter()
            .filter_map(|pane| {
                Some((
                    pane.get("pane_id")?.as_str()?.to_owned(),
                    Rect::from(pane.get("rect")?)?,
                ))
            })
            .collect();
        let splits = layout
            .get("splits")
            .and_then(Value::as_array)
            .map(|splits| {
                splits
                    .iter()
                    .filter_map(|split| {
                        Some(Split {
                            down: split.get("direction")?.as_str()? == "down",
                            ratio: split.get("ratio")?.as_f64()?,
                            rect: Rect::from(split.get("rect")?)?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { panes, splits })
    }

    fn rect(&self, pane: &str) -> Option<Rect> {
        self.panes
            .iter()
            .find(|(id, _)| id == pane)
            .map(|(_, rect)| *rect)
    }
}

/// Where the next worker opens.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Placement {
    Split {
        pane: String,
        direction: &'static str,
    },
    /// The area has no room left for another readable pane.
    Tab,
}

/// Workers that share a top edge, left to right.
struct Row {
    panes: Vec<(String, Rect)>,
}

impl Row {
    fn x(&self) -> u64 {
        self.panes[0].1.x
    }

    fn y(&self) -> u64 {
        self.panes[0].1.y
    }

    fn width(&self) -> u64 {
        self.panes.iter().map(|(_, rect)| rect.width).sum()
    }

    fn height(&self) -> u64 {
        self.panes
            .iter()
            .map(|(_, rect)| rect.height)
            .max()
            .unwrap_or(0)
    }
}

/// The caller's workers that are still in its tab, grouped into rows. Workers
/// elsewhere, such as a pane the user moved, are left alone.
fn rows(layout: &Layout, caller: &str, workers: &[String]) -> Vec<Row> {
    let Some(caller) = layout.rect(caller) else {
        return Vec::new();
    };
    let mut panes: Vec<(String, Rect)> = workers
        .iter()
        .filter_map(|worker| Some((worker.clone(), layout.rect(worker)?)))
        .filter(|(_, rect)| rect.x >= caller.x + caller.width || rect.y >= caller.y + caller.height)
        .collect();
    panes.sort_by_key(|(_, rect)| (rect.y, rect.x));
    let mut rows: Vec<Row> = Vec::new();
    for (id, rect) in panes {
        if let Some(row) = rows.last_mut()
            && row.y() == rect.y
        {
            row.panes.push((id, rect));
            continue;
        }
        rows.push(Row {
            panes: vec![(id, rect)],
        });
    }
    rows
}

pub(super) fn place(layout: &Layout, caller: &str, workers: &[String]) -> Placement {
    let rows = rows(layout, caller, workers);
    let Some(first) = rows.first() else {
        let narrow = layout
            .rect(caller)
            .is_some_and(|rect| rect.width < NARROW_CALLER);
        return Placement::Split {
            pane: caller.to_owned(),
            direction: if narrow { "down" } else { "right" },
        };
    };
    let height: u64 = rows.iter().map(Row::height).sum();
    let count = rows.len() as u64;
    let pairable = first.width() / 2 >= MIN_COLS;
    let min_rows = if pairable { MIN_ROWS_PAIRED } else { MIN_ROWS };
    if let Some(last) = rows.last()
        && last.panes.len() == 1
        && height / (count + 1) >= min_rows
    {
        return Placement::Split {
            pane: last.panes[0].0.clone(),
            direction: "down",
        };
    }
    if pairable
        && height / count >= MIN_ROWS_PAIRED
        && let Some(row) = rows.iter().find(|row| row.panes.len() == 1)
    {
        return Placement::Split {
            pane: row.panes[0].0.clone(),
            direction: "right",
        };
    }
    Placement::Tab
}

/// The caller's workers in the row directly under it, left to right. Only
/// panes inside the caller's own column count.
fn row_below(layout: &Layout, caller: &str, workers: &[String]) -> Vec<(String, Rect)> {
    let Some(caller) = layout.rect(caller) else {
        return Vec::new();
    };
    let mut row: Vec<(String, Rect)> = workers
        .iter()
        .filter_map(|worker| Some((worker.clone(), layout.rect(worker)?)))
        .filter(|(_, rect)| {
            rect.y == caller.y + caller.height
                && rect.x >= caller.x
                && rect.x + rect.width <= caller.x + caller.width
        })
        .collect();
    row.sort_by_key(|(_, rect)| rect.x);
    row
}

/// Rows a pane of this width needs to show a startup dialog whole.
fn rows_needed(width: u64) -> u64 {
    if width < NARROW_CALLER {
        MIN_ROWS_PAIRED
    } else {
        MIN_ROWS
    }
}

pub(super) fn place_below(layout: &Layout, caller: &str, workers: &[String]) -> Placement {
    let row = row_below(layout, caller, workers);
    let Some((last, _)) = row.last() else {
        let Some(rect) = layout.rect(caller) else {
            return Placement::Tab;
        };
        if rect.width < MIN_COLS || rect.height / 2 < rows_needed(rect.width) {
            return Placement::Tab;
        }
        return Placement::Split {
            pane: caller.to_owned(),
            direction: "down",
        };
    };
    let width: u64 = row.iter().map(|(_, rect)| rect.width).sum();
    let height = row.iter().map(|(_, rect)| rect.height).min().unwrap_or(0);
    let each = width / (row.len() as u64 + 1);
    if each < MIN_COLS || height < rows_needed(each) {
        return Placement::Tab;
    }
    Placement::Split {
        pane: last.clone(),
        direction: "right",
    }
}

/// The resizes that make the caller's workers equal, as
/// `(pane, direction, amount)` for `herdr pane resize`.
pub(super) fn equalize(
    layout: &Layout,
    caller: &str,
    workers: &[String],
) -> Vec<(String, &'static str, f64)> {
    let rows = rows(layout, caller, workers);
    let count = rows.len();
    let mut moves = Vec::new();
    for (index, row) in rows.iter().enumerate().take(count.saturating_sub(1)) {
        // The split whose first child is this row: it starts where the row
        // starts, is as wide, and is taller.
        let Some(split) = layout
            .splits
            .iter()
            .filter(|split| {
                split.down
                    && split.rect.x == row.x()
                    && split.rect.y == row.y()
                    && split.rect.width == row.width()
                    && split.rect.height > row.height()
            })
            .min_by_key(|split| split.rect.height)
        else {
            continue;
        };
        #[allow(clippy::cast_precision_loss)]
        let target = 1.0 / (count - index) as f64;
        let change = target - split.ratio;
        if change > TOLERANCE {
            moves.push((row.panes[0].0.clone(), "down", change));
        } else if change < -TOLERANCE {
            moves.push((rows[index + 1].panes[0].0.clone(), "up", -change));
        }
    }
    for row in &rows {
        let len = row.panes.len();
        if len < 2 {
            continue;
        }
        for (index, (pane, rect)) in row.panes.iter().enumerate().take(len.saturating_sub(1)) {
            let Some(split) = layout
                .splits
                .iter()
                .filter(|split| {
                    !split.down
                        && split.rect.x == rect.x
                        && split.rect.y == rect.y
                        && split.rect.height == rect.height
                        && split.rect.width > rect.width
                })
                .min_by_key(|split| split.rect.width)
            else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)]
            let target = 1.0 / (len - index) as f64;
            let change = target - split.ratio;
            if change > TOLERANCE {
                moves.push((pane.clone(), "right", change));
            } else if change < -TOLERANCE {
                moves.push((row.panes[index + 1].0.clone(), "left", -change));
            }
        }
    }
    moves
}

/// Panes brgr opened for this caller and has not closed.
pub(super) fn workers(runs: &Path, caller: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(runs) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".pane.json"))
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<PaneReceipt>(&bytes).ok())
        .filter(|receipt| receipt.caller.as_deref() == Some(caller) && receipt.cleanup != "closed")
        .map(|receipt| receipt.pane)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pane(id: &str, x: u64, y: u64, width: u64, height: u64) -> Value {
        json!({"pane_id": id, "rect": {"x": x, "y": y, "width": width, "height": height}})
    }

    fn split(direction: &str, ratio: f64, x: u64, y: u64, width: u64, height: u64) -> Value {
        json!({"direction": direction, "ratio": ratio,
               "rect": {"x": x, "y": y, "width": width, "height": height}})
    }

    fn layout(panes: &[Value], splits: &[Value]) -> Layout {
        Layout::parse(&json!({"result": {"layout": {"panes": panes, "splits": splits}}})).unwrap()
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_first_worker_splits_the_caller_by_its_width() {
        let wide = layout(&[pane("c", 0, 0, 208, 66)], &[]);
        assert_eq!(
            place(&wide, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "right"
            }
        );
        let narrow = layout(&[pane("c", 0, 0, 90, 66)], &[]);
        assert_eq!(
            place(&narrow, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "down"
            }
        );
    }

    /// Seen in Herdr 0.9: caller left, three workers stacked on the right.
    fn three_stacked(ratios: [f64; 2]) -> Layout {
        layout(
            &[
                pane("c", 0, 0, 104, 66),
                pane("a", 104, 0, 104, 33),
                pane("b", 104, 33, 104, 17),
                pane("d", 104, 50, 104, 16),
            ],
            &[
                split("right", 0.5, 0, 0, 208, 66),
                split("down", ratios[0], 104, 0, 104, 66),
                split("down", ratios[1], 104, 33, 104, 33),
            ],
        )
    }

    #[test]
    fn later_workers_stack_under_the_last_worker_not_the_caller() {
        // Tall enough that a fourth stacked worker keeps MIN_ROWS.
        let layout = layout(
            &[
                pane("c", 0, 0, 104, 132),
                pane("a", 104, 0, 104, 66),
                pane("b", 104, 66, 104, 34),
                pane("d", 104, 100, 104, 32),
            ],
            &[
                split("right", 0.5, 0, 0, 208, 132),
                split("down", 0.5, 104, 0, 104, 132),
                split("down", 0.5, 104, 66, 104, 66),
            ],
        );
        assert_eq!(
            place(&layout, "c", &names(&["a", "b", "d"])),
            Placement::Split {
                pane: "d".into(),
                direction: "down"
            }
        );
    }

    #[test]
    fn a_chain_of_halves_becomes_thirds() {
        let layout = three_stacked([0.5, 0.5]);
        let moves = equalize(&layout, "c", &names(&["d", "a", "b"]));
        assert_eq!(moves.len(), 1);
        // The top split shrinks from 1/2 to 1/3 by moving the border above b up.
        assert_eq!(moves[0].0, "b");
        assert_eq!(moves[0].1, "up");
        assert!((moves[0].2 - (0.5 - 1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn equal_workers_need_no_resize_and_unknown_panes_are_ignored() {
        let layout = three_stacked([1.0 / 3.0, 0.5]);
        assert!(equalize(&layout, "c", &names(&["a", "b", "d", "gone"])).is_empty());
        // A pane brgr did not open is never part of the area.
        assert!(equalize(&layout, "c", &names(&["a"])).is_empty());
    }

    #[test]
    fn a_grown_lower_split_is_raised_from_its_upper_pane() {
        let layout = three_stacked([1.0 / 3.0, 0.3]);
        let moves = equalize(&layout, "c", &names(&["a", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "down"));
        assert!((moves[0].2 - 0.2).abs() < 1e-9);
    }

    #[test]
    fn workers_below_a_narrow_caller_stack_under_it() {
        let layout = layout(
            &[
                pane("c", 0, 0, 90, 66),
                pane("a", 0, 66, 90, 44),
                pane("b", 0, 110, 90, 22),
            ],
            &[
                split("down", 0.5, 0, 0, 90, 132),
                split("down", 0.66, 0, 66, 90, 66),
            ],
        );
        assert_eq!(
            place(&layout, "c", &names(&["a", "b"])),
            Placement::Split {
                pane: "b".into(),
                direction: "down"
            }
        );
        let moves = equalize(&layout, "c", &names(&["a", "b"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "up"));
        assert!((moves[0].2 - 0.16).abs() < 1e-9);
    }

    /// A 58-row column holding three stacked workers would give a fourth about
    /// 14 rows. Claude Code's folder-trust dialog then loses its
    /// `Accessing workspace:` line and path off the top, brgr cannot match the
    /// path, and the worker sits on the dialog (seen live, 2026-11-13).
    #[test]
    fn a_split_too_short_for_the_trust_dialog_goes_to_a_tab() {
        let panes: Vec<Value> = (0..3)
            .map(|index| pane(&format!("w{index}"), 104, index * 19, 104, 19))
            .chain([pane("c", 0, 0, 104, 58)])
            .collect();
        let layout = layout(&panes, &[]);
        let workers: Vec<String> = (0..3).map(|index| format!("w{index}")).collect();
        assert_eq!(place(&layout, "c", &workers), Placement::Tab);
    }

    #[test]
    fn a_full_area_sends_the_next_worker_to_a_tab() {
        let panes: Vec<Value> = (0..6)
            .map(|index| pane(&format!("w{index}"), 104, index * 11, 104, 11))
            .chain([pane("c", 0, 0, 104, 66)])
            .collect();
        let layout = layout(&panes, &[]);
        let workers: Vec<String> = (0..6).map(|index| format!("w{index}")).collect();
        assert_eq!(place(&layout, "c", &workers), Placement::Tab);
    }

    fn two_stacked_57() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 57),
                pane("a", 119, 0, 119, 28),
                pane("b", 119, 28, 119, 29),
            ],
            &[],
        )
    }

    fn two_stacked_72() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 72),
                pane("a", 119, 0, 119, 36),
                pane("b", 119, 36, 119, 36),
            ],
            &[],
        )
    }

    fn three_rows_second_column_started() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 72),
                pane("a", 119, 0, 60, 24),
                pane("e", 179, 0, 59, 24),
                pane("b", 119, 24, 119, 24),
                pane("d", 119, 48, 119, 24),
            ],
            &[],
        )
    }

    fn three_rows_second_column_on_row_two() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 72),
                pane("a", 119, 0, 60, 24),
                pane("e", 179, 0, 59, 24),
                pane("b", 119, 24, 60, 24),
                pane("f", 179, 24, 59, 24),
                pane("d", 119, 48, 119, 24),
            ],
            &[],
        )
    }

    fn three_pair_rows() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 72),
                pane("a", 119, 0, 60, 24),
                pane("e", 179, 0, 59, 24),
                pane("b", 119, 24, 60, 24),
                pane("f", 179, 24, 59, 24),
                pane("d", 119, 48, 60, 24),
                pane("g", 179, 48, 59, 24),
            ],
            &[],
        )
    }

    fn closed_left_of_row_one() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 131, 72),
                pane("f", 131, 0, 107, 40),
                pane("b", 131, 40, 54, 16),
                pane("g", 185, 40, 53, 16),
                pane("d", 131, 56, 54, 16),
                pane("h", 185, 56, 53, 16),
            ],
            &[
                split("right", 0.55, 0, 0, 238, 72),
                split("down", 0.55, 131, 0, 107, 72),
                split("down", 0.5, 131, 40, 107, 32),
                split("right", 0.5, 131, 40, 107, 16),
                split("right", 0.5, 131, 56, 107, 16),
            ],
        )
    }

    fn closed_whole_row() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 131, 72),
                pane("f", 131, 0, 107, 40),
                pane("d", 131, 40, 54, 32),
                pane("h", 185, 40, 53, 32),
            ],
            &[
                split("right", 0.55, 0, 0, 238, 72),
                split("down", 0.55, 131, 0, 107, 72),
                split("right", 0.5, 131, 40, 107, 32),
            ],
        )
    }

    fn finding_two(top_down: f64, pair: f64) -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 72),
                pane("a", 119, 0, 60, 36),
                pane("e", 179, 0, 59, 36),
                pane("b", 119, 36, 119, 18),
                pane("d", 119, 54, 119, 18),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", top_down, 119, 0, 119, 72),
                split("right", pair, 119, 0, 119, 36),
                split("down", 0.5, 119, 36, 119, 36),
            ],
        )
    }

    fn equal_two_by_two(root: f64) -> Layout {
        layout(
            &[
                pane("c", 0, 0, 131, 72),
                pane("a", 131, 0, 54, 36),
                pane("e", 185, 0, 53, 36),
                pane("b", 131, 36, 54, 36),
                pane("f", 185, 36, 53, 36),
            ],
            &[
                split("right", root, 0, 0, 238, 72),
                split("down", 0.5, 131, 0, 107, 72),
                split("right", 0.5, 131, 0, 107, 36),
                split("right", 0.5, 131, 36, 107, 36),
            ],
        )
    }

    fn assert_horizontal_resizes_stay_inside_the_row(
        layout: &Layout,
        caller: &str,
        workers: &[&str],
    ) {
        let names = names(workers);
        let rows = rows(layout, caller, &names);
        for (pane, direction, _) in equalize(layout, caller, &names) {
            if direction != "right" && direction != "left" {
                continue;
            }
            let row = rows
                .iter()
                .find(|row| row.panes.iter().any(|(id, _)| id == &pane))
                .unwrap_or_else(|| panic!("move {pane} is not a worker"));
            let index = row.panes.iter().position(|(id, _)| id == &pane).unwrap();
            if direction == "right" {
                assert!(
                    index + 1 < row.panes.len(),
                    "right without a worker to the right: {pane}"
                );
            } else {
                assert!(index > 0, "left without a worker to the left: {pane}");
            }
        }
    }

    #[test]
    fn a_full_column_starts_a_second_column_beside_the_first_worker() {
        assert_eq!(
            place(&two_stacked_57(), "c", &names(&["a", "b"])),
            Placement::Split {
                pane: "a".into(),
                direction: "right"
            }
        );
    }

    #[test]
    fn a_third_row_is_added_before_the_second_column_when_it_fits() {
        assert_eq!(
            place(&two_stacked_72(), "c", &names(&["a", "b"])),
            Placement::Split {
                pane: "b".into(),
                direction: "down"
            }
        );
    }

    #[test]
    fn the_second_column_fills_top_to_bottom() {
        assert_eq!(
            place(
                &three_rows_second_column_started(),
                "c",
                &names(&["a", "e", "b", "d"])
            ),
            Placement::Split {
                pane: "b".into(),
                direction: "right"
            }
        );
        assert_eq!(
            place(
                &three_rows_second_column_on_row_two(),
                "c",
                &names(&["a", "e", "b", "f", "d"])
            ),
            Placement::Split {
                pane: "d".into(),
                direction: "right"
            }
        );
    }

    #[test]
    fn both_columns_full_sends_the_next_worker_to_a_tab() {
        assert_eq!(
            place(
                &three_pair_rows(),
                "c",
                &names(&["a", "e", "b", "f", "d", "g"])
            ),
            Placement::Tab
        );
    }

    #[test]
    fn an_area_too_narrow_for_two_columns_goes_to_a_tab() {
        let layout = layout(
            &[
                pane("c", 0, 0, 100, 57),
                pane("a", 100, 0, 98, 29),
                pane("b", 100, 29, 98, 28),
            ],
            &[],
        );
        assert_eq!(place(&layout, "c", &names(&["a", "b"])), Placement::Tab);
    }

    #[test]
    fn rows_too_short_for_a_half_width_trust_dialog_are_not_paired() {
        let layout = layout(
            &[
                pane("c", 0, 0, 119, 66),
                pane("a", 119, 0, 119, 22),
                pane("b", 119, 22, 119, 22),
                pane("d", 119, 44, 119, 22),
            ],
            &[],
        );
        assert_eq!(
            place(&layout, "c", &names(&["a", "b", "d"])),
            Placement::Tab
        );
    }

    #[test]
    fn a_later_worker_never_splits_the_caller() {
        for (current, workers) in [
            (two_stacked_57(), names(&["a", "b"])),
            (two_stacked_72(), names(&["a", "b"])),
            (
                three_rows_second_column_started(),
                names(&["a", "e", "b", "d"]),
            ),
            (three_pair_rows(), names(&["a", "e", "b", "f", "d", "g"])),
        ] {
            match place(&current, "c", &workers) {
                Placement::Tab => {}
                Placement::Split { pane, .. } => {
                    assert_ne!(pane, "c", "a later worker must not split the caller");
                }
            }
        }
    }

    #[test]
    fn a_closed_worker_leaves_its_row_to_be_filled_next() {
        let layout = closed_left_of_row_one();
        let workers = names(&["f", "b", "g", "d", "h"]);
        assert_eq!(
            place(&layout, "c", &workers),
            Placement::Split {
                pane: "f".into(),
                direction: "right"
            }
        );
        let moves = equalize(&layout, "c", &workers);
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "up"));
        assert!((moves[0].2 - (0.55 - 1.0 / 3.0)).abs() < 1e-9);
        assert_horizontal_resizes_stay_inside_the_row(&layout, "c", &["f", "b", "g", "d", "h"]);
    }

    #[test]
    fn a_closed_row_makes_the_remaining_rows_equal() {
        let layout = closed_whole_row();
        let moves = equalize(&layout, "c", &names(&["f", "d", "h"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("d", "up"));
        assert!((moves[0].2 - 0.05).abs() < 1e-9);
        assert_horizontal_resizes_stay_inside_the_row(&layout, "c", &["f", "d", "h"]);
    }

    #[test]
    fn a_row_of_two_is_resized_as_one_row() {
        let layout = finding_two(0.5, 0.5);
        let moves = equalize(&layout, "c", &names(&["a", "e", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "up"));
        assert!((moves[0].2 - (0.5 - 1.0 / 3.0)).abs() < 1e-9);
        let layout = finding_two(0.2, 0.5);
        let moves = equalize(&layout, "c", &names(&["a", "e", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("a", "down"));
        assert!((moves[0].2 - (1.0 / 3.0 - 0.2)).abs() < 1e-9);
        assert_horizontal_resizes_stay_inside_the_row(&layout, "c", &["a", "e", "b", "d"]);
    }

    #[test]
    fn an_uneven_pair_is_made_equal() {
        let left_heavy = finding_two(1.0 / 3.0, 0.6);
        let moves = equalize(&left_heavy, "c", &names(&["a", "e", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("e", "left"));
        assert!((moves[0].2 - 0.1).abs() < 1e-9);
        assert_horizontal_resizes_stay_inside_the_row(&left_heavy, "c", &["a", "e", "b", "d"]);
        let right_heavy = finding_two(1.0 / 3.0, 0.4);
        let moves = equalize(&right_heavy, "c", &names(&["a", "e", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("a", "right"));
        assert!((moves[0].2 - 0.1).abs() < 1e-9);
        assert_horizontal_resizes_stay_inside_the_row(&right_heavy, "c", &["a", "e", "b", "d"]);
    }

    #[test]
    fn the_callers_border_is_never_resized() {
        let layout = equal_two_by_two(0.55);
        assert!(equalize(&layout, "c", &names(&["a", "e", "b", "f"])).is_empty());
        assert_horizontal_resizes_stay_inside_the_row(&layout, "c", &["a", "e", "b", "f"]);
        assert_horizontal_resizes_stay_inside_the_row(
            &closed_left_of_row_one(),
            "c",
            &["f", "b", "g", "d", "h"],
        );
        assert_horizontal_resizes_stay_inside_the_row(&closed_whole_row(), "c", &["f", "d", "h"]);
        assert_horizontal_resizes_stay_inside_the_row(
            &finding_two(0.5, 0.5),
            "c",
            &["a", "e", "b", "d"],
        );
        assert_horizontal_resizes_stay_inside_the_row(
            &finding_two(0.2, 0.5),
            "c",
            &["a", "e", "b", "d"],
        );
        assert_horizontal_resizes_stay_inside_the_row(
            &finding_two(1.0 / 3.0, 0.6),
            "c",
            &["a", "e", "b", "d"],
        );
        assert_horizontal_resizes_stay_inside_the_row(
            &finding_two(1.0 / 3.0, 0.4),
            "c",
            &["a", "e", "b", "d"],
        );
    }

    #[test]
    fn all_rows_paired_with_room_for_a_row_goes_to_a_tab() {
        let layout = layout(
            &[
                pane("c", 0, 0, 119, 72),
                pane("a", 119, 0, 60, 36),
                pane("e", 179, 0, 59, 36),
                pane("b", 119, 36, 60, 36),
                pane("f", 179, 36, 59, 36),
            ],
            &[],
        );
        assert_eq!(
            place(&layout, "c", &names(&["a", "e", "b", "f"])),
            Placement::Tab
        );
    }
    fn two_heads_rows() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 32),
                pane("a", 0, 32, 40, 40),
                pane("b", 40, 32, 40, 40),
                pane("d", 80, 32, 39, 40),
                pane("h", 119, 0, 119, 36),
                pane("e", 119, 36, 60, 36),
                pane("f", 179, 36, 59, 36),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", 0.45, 0, 0, 119, 72),
                split("right", 0.3333, 0, 32, 119, 40),
                split("right", 0.5, 40, 32, 79, 40),
                split("down", 0.5, 119, 0, 119, 72),
                split("right", 0.5, 119, 36, 119, 36),
            ],
        )
    }

    fn row_of_three_halves() -> Layout {
        layout(
            &[
                pane("c", 0, 0, 119, 36),
                pane("a", 0, 36, 60, 36),
                pane("b", 60, 36, 30, 36),
                pane("d", 90, 36, 29, 36),
                pane("h", 119, 0, 119, 72),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", 0.5, 0, 0, 119, 72),
                split("right", 0.5, 0, 36, 119, 36),
                split("right", 0.5, 60, 36, 59, 36),
            ],
        )
    }

    #[test]
    fn below_the_first_worker_splits_the_caller_down() {
        let wide = layout(&[pane("c", 0, 0, 238, 72)], &[]);
        assert_eq!(
            place_below(&wide, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "down"
            }
        );
        let two_heads = layout(&[pane("c", 0, 0, 119, 72), pane("h", 119, 0, 119, 72)], &[]);
        assert_eq!(
            place_below(&two_heads, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "down"
            }
        );
    }

    #[test]
    fn below_later_workers_split_the_last_worker_right() {
        let two = layout(
            &[
                pane("c", 0, 0, 238, 36),
                pane("a", 0, 36, 119, 36),
                pane("b", 119, 36, 119, 36),
            ],
            &[],
        );
        assert_eq!(
            place_below(&two, "c", &names(&["a", "b"])),
            Placement::Split {
                pane: "b".into(),
                direction: "right"
            }
        );
        let one = layout(&[pane("c", 0, 0, 238, 36), pane("a", 0, 36, 238, 36)], &[]);
        assert_eq!(
            place_below(&one, "c", &names(&["a"])),
            Placement::Split {
                pane: "a".into(),
                direction: "right"
            }
        );
    }

    #[test]
    fn below_a_row_of_halves_becomes_thirds() {
        let moves = equalize(&row_of_three_halves(), "c", &names(&["a", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "left"));
        assert!((moves[0].2 - (0.5 - 1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn below_never_resizes_the_caller_or_another_heads_border() {
        let current = two_heads_rows();
        assert!(equalize(&current, "c", &names(&["a", "b", "d"])).is_empty());
        assert!(equalize(&current, "h", &names(&["e", "f"])).is_empty());
        let uneven = layout(
            &[
                pane("c", 0, 0, 119, 32),
                pane("a", 0, 32, 40, 40),
                pane("b", 40, 32, 40, 40),
                pane("d", 80, 32, 39, 40),
                pane("h", 119, 0, 119, 36),
                pane("e", 119, 36, 60, 36),
                pane("f", 179, 36, 59, 36),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", 0.45, 0, 0, 119, 72),
                split("right", 0.3333, 0, 32, 119, 40),
                split("right", 0.5, 40, 32, 79, 40),
                split("down", 0.5, 119, 0, 119, 72),
                split("right", 0.6, 119, 36, 119, 36),
            ],
        );
        let moves = equalize(&uneven, "h", &names(&["e", "f"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("f", "left"));
        assert!((moves[0].2 - 0.1).abs() < 1e-9);
        for (pane, direction, _) in moves
            .iter()
            .cloned()
            .chain(equalize(&current, "c", &names(&["a", "b", "d"])))
            .chain(equalize(&current, "h", &names(&["e", "f"])))
        {
            assert_ne!(direction, "up");
            assert_ne!(direction, "down");
            assert_ne!(pane, "c");
            assert_ne!(pane, "h");
        }
        assert_horizontal_resizes_stay_inside_the_row(&current, "c", &["a", "b", "d"]);
        assert_horizontal_resizes_stay_inside_the_row(&current, "h", &["e", "f"]);
        assert_horizontal_resizes_stay_inside_the_row(&uneven, "h", &["e", "f"]);
    }

    #[test]
    fn below_two_heads_each_keep_their_own_row() {
        let under_h = layout(
            &[
                pane("c", 0, 0, 119, 32),
                pane("a", 0, 32, 40, 40),
                pane("b", 40, 32, 40, 40),
                pane("d", 80, 32, 39, 40),
                pane("h", 119, 0, 119, 36),
                pane("e", 119, 36, 119, 36),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", 0.45, 0, 0, 119, 72),
                split("right", 0.3333, 0, 32, 119, 40),
                split("right", 0.5, 40, 32, 79, 40),
                split("down", 0.5, 119, 0, 119, 72),
            ],
        );
        match place_below(&under_h, "h", &names(&["e", "a", "b", "d"])) {
            Placement::Split { pane, direction } => {
                assert_eq!(pane, "e");
                assert_eq!(direction, "right");
                assert_ne!(pane, "a");
                assert_ne!(pane, "b");
                assert_ne!(pane, "d");
                assert_ne!(pane, "c");
            }
            Placement::Tab => panic!("expected a split of e, not a tab"),
        }
        let under_c = layout(
            &[
                pane("c", 0, 0, 119, 36),
                pane("a", 0, 36, 119, 36),
                pane("h", 119, 0, 119, 36),
                pane("e", 119, 36, 119, 36),
            ],
            &[],
        );
        match place_below(&under_c, "c", &names(&["a", "e"])) {
            Placement::Split { pane, direction } => {
                assert_eq!(pane, "a");
                assert_eq!(direction, "right");
                assert_ne!(pane, "h");
                assert_ne!(pane, "e");
            }
            Placement::Tab => panic!("expected a split of a, not a tab"),
        }
        match place_below(&under_c, "h", &names(&["e", "a"])) {
            Placement::Split { pane, direction } => {
                assert_eq!(pane, "e");
                assert_eq!(direction, "right");
                assert_ne!(pane, "a");
                assert_ne!(pane, "c");
            }
            Placement::Tab => panic!("expected a split of e, not a tab"),
        }
    }

    #[test]
    fn below_a_full_row_sends_the_next_worker_to_a_tab() {
        assert_eq!(
            place_below(&two_heads_rows(), "h", &names(&["e", "f"])),
            Placement::Tab
        );
        let three = layout(
            &[
                pane("c", 0, 0, 238, 36),
                pane("a", 0, 36, 80, 36),
                pane("b", 80, 36, 79, 36),
                pane("d", 159, 36, 79, 36),
            ],
            &[],
        );
        assert_eq!(
            place_below(&three, "c", &names(&["a", "b", "d"])),
            Placement::Split {
                pane: "d".into(),
                direction: "right"
            }
        );
        let four = layout(
            &[
                pane("c", 0, 0, 238, 36),
                pane("a", 0, 36, 60, 36),
                pane("b", 60, 36, 60, 36),
                pane("d", 120, 36, 59, 36),
                pane("e", 179, 36, 59, 36),
            ],
            &[],
        );
        assert_eq!(
            place_below(&four, "c", &names(&["a", "b", "d", "e"])),
            Placement::Tab
        );
    }

    #[test]
    fn below_a_narrow_caller_gets_one_worker_then_tabs() {
        let one = layout(&[pane("c", 0, 0, 90, 40), pane("a", 0, 40, 90, 40)], &[]);
        assert_eq!(place_below(&one, "c", &names(&["a"])), Placement::Tab);
        let empty = layout(&[pane("c", 0, 0, 90, 72)], &[]);
        assert_eq!(
            place_below(&empty, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "down"
            }
        );
    }

    #[test]
    fn below_a_caller_too_short_to_halve_goes_to_a_tab() {
        let short = layout(&[pane("c", 0, 0, 238, 38)], &[]);
        assert_eq!(place_below(&short, "c", &[]), Placement::Tab);
        let narrow_short = layout(&[pane("c", 0, 0, 90, 44)], &[]);
        assert_eq!(place_below(&narrow_short, "c", &[]), Placement::Tab);
        let tall_enough = layout(&[pane("c", 0, 0, 238, 44)], &[]);
        assert_eq!(
            place_below(&tall_enough, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "down"
            }
        );
    }

    #[test]
    fn below_a_row_too_short_for_narrow_panes_is_not_divided_further() {
        let pair = layout(
            &[
                pane("c", 0, 0, 238, 22),
                pane("a", 0, 22, 119, 22),
                pane("b", 119, 22, 119, 22),
            ],
            &[],
        );
        assert_eq!(place_below(&pair, "c", &names(&["a", "b"])), Placement::Tab);
        let one = layout(&[pane("c", 0, 0, 238, 22), pane("a", 0, 22, 238, 22)], &[]);
        assert_eq!(
            place_below(&one, "c", &names(&["a"])),
            Placement::Split {
                pane: "a".into(),
                direction: "right"
            }
        );
    }

    #[test]
    fn below_a_closed_worker_leaves_the_rest_equal() {
        let closed_middle = layout(
            &[
                pane("c", 0, 0, 119, 36),
                pane("a", 0, 36, 40, 36),
                pane("d", 40, 36, 79, 36),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", 0.5, 0, 0, 119, 72),
                split("right", 1.0 / 3.0, 0, 36, 119, 36),
            ],
        );
        let moves = equalize(&closed_middle, "c", &names(&["a", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("a", "right"));
        assert!((moves[0].2 - (0.5 - 1.0 / 3.0)).abs() < 1e-9);
        let closed_first = layout(
            &[
                pane("c", 0, 0, 119, 36),
                pane("d", 0, 36, 60, 36),
                pane("f", 60, 36, 59, 36),
            ],
            &[
                split("right", 0.5, 0, 0, 238, 72),
                split("down", 0.5, 0, 0, 119, 72),
                split("right", 0.5, 0, 36, 119, 36),
            ],
        );
        assert!(equalize(&closed_first, "c", &names(&["d", "f"])).is_empty());
    }

    #[test]
    fn below_ignores_own_workers_outside_the_callers_column() {
        let layout = layout(
            &[
                pane("c", 0, 0, 119, 36),
                pane("a", 0, 36, 119, 36),
                pane("r", 119, 0, 119, 72),
            ],
            &[],
        );
        assert_eq!(
            place_below(&layout, "c", &names(&["a", "r"])),
            Placement::Split {
                pane: "a".into(),
                direction: "right"
            }
        );
    }
}
