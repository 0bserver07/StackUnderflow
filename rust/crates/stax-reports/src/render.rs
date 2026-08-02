//! `reports/render.py` — the presentation layer, Rich included.
//!
//! # Why a Rich port lives here
//!
//! `render_text` is nine lines of Python and one import:
//!
//! ```python
//! console = Console(file=stream, force_terminal=False, highlight=False)
//! console.print(f"[bold]StackUnderflow — {report['scope_label']}[/bold]")
//! table = Table(show_header=True, header_style="bold")
//! …
//! console.print(table)
//! ```
//!
//! …and it is the DEFAULT output of `report`, `today` and `month`. Byte parity
//! on those three verbs is byte parity with Rich's table layout engine, so the
//! layout engine is what gets ported: [`rich::fit_widths`] is
//! `Table._calculate_column_widths` + `Table._collapse_widths` +
//! `_ratio.ratio_reduce`, transcribed from Rich's source rather than inferred
//! from screenshots.
//!
//! Three things about that engine are counter-intuitive enough to be worth
//! naming, because each one is a place a "reasonable" reimplementation diverges:
//!
//! 1. **Column widths include their own padding and exclude the borders.**
//!    `_measure_column` measures a `Padding(cell, (0,1,0,1))`, so a column whose
//!    widest cell is 5 cells wide measures 7. The borders are subtracted from
//!    the budget up front instead (`__rich_console__` does
//!    `max_width -= len(columns) - 1` and another 2 for the edge), which is why
//!    [`rich::fit_widths`] takes the budget already reduced.
//! 2. **Over-wide tables shrink the widest column down to the second-widest,
//!    then both together, then all three** — `_collapse_widths` is a loop, not a
//!    proportional squeeze, and it distributes with
//!    `round(ratio * remaining / total_ratio)`. That `round` is CPython's, so it
//!    is **round-half-to-even**: [`py_round_half_even`] exists for exactly one
//!    call site and it is that one. A `.round()` there is right on most inputs
//!    and off by a cell on the ties, which is a whole column of box-drawing
//!    characters in the wrong place.
//! 3. **Truncation counts cells, not bytes**, and the ellipsis is U+2026 — one
//!    character replacing the last one, so an over-long cell renders as
//!    `width - 1` characters plus `…`.
//!
//! # What is deliberately NOT ported
//!
//! Colour, styles and markup *semantics*. `force_terminal=False` with no TTY
//! means every style is dropped before a byte is written, so [`rich::print_text`]
//! strips the tags and keeps the text. That is a faithful port of the *output*,
//! not of Rich — and the distinction is recorded rather than hidden, because a
//! caller who one day passes `force_terminal=True` would get plain text here and
//! ANSI there.
//!
//! The markup stripper handles the closed `[tag]…[/tag]` shape the three call
//! sites in `render.py` use. A `scope_label` containing a literal `[` would be
//! parsed as markup by Rich and left alone by this port — recorded as a
//! divergence rather than guessed at, because every label `parse_period` can
//! produce ("today", "July 2026", "last 7 days", "all time") is bracket-free and
//! a fabricated escape rule would be a second thing to keep true.

use serde_json::Value;

use crate::aggregate::Report;

/// The console width Rich falls back to with no terminal — and what `$COLUMNS`
/// overrides. `Console.width` reads `COLUMNS` before defaulting to 80.
pub const DEFAULT_CONSOLE_WIDTH: usize = 80;

/// `render_text(report, stream=None)` — the header line, the table, the total.
///
/// `width` is `Console.width`: `$COLUMNS` when set, else
/// [`DEFAULT_CONSOLE_WIDTH`].
#[must_use]
pub fn render_text(report: &Report, width: usize) -> String {
    let mut out = String::new();
    out.push_str(&rich::print_text(
        &format!("StackUnderflow — {}", report.scope_label),
        width,
    ));

    if report.by_project.is_empty() {
        // `[dim]No activity in this period.[/dim]` — one `console.print`, then
        // the total. The early return means NO table is emitted at all, not an
        // empty one.
        out.push_str(&rich::print_text("No activity in this period.", width));
        out.push_str(&rich::print_text(&total_line(report), width));
        return out;
    }

    let mut table = rich::Table::new(&[
        ("Project", rich::Justify::Left),
        ("Cost", rich::Justify::Right),
        ("Messages", rich::Justify::Right),
        ("Sessions", rich::Justify::Right),
    ]);
    for row in &report.by_project {
        table.add_row(&[
            row.name.clone(),
            format!("${:.2}", row.cost),
            py_thousands(row.messages),
            py_thousands(row.sessions),
        ]);
    }
    out.push_str(&table.render(width));
    out.push_str(&rich::print_text(&total_line(report), width));
    out
}

/// The `Total:` line, with `[bold]` already gone.
///
/// `f"[bold]Total:[/bold] ${…:.2f}  {…:,} messages  {…:,} sessions"` — two
/// spaces between the three parts, and the two counts ARE thousands-separated
/// (unlike `render_status_line`'s, which are not).
#[must_use]
pub fn total_line(report: &Report) -> String {
    format!(
        "Total: ${:.2}  {} messages  {} sessions",
        report.total_cost,
        py_thousands(report.total_messages),
        py_thousands(report.total_sessions),
    )
}

/// `render_json(report)` — `json.dumps(report, indent=2, sort_keys=False)`.
///
/// The writer is the CLI's (`ensure_ascii=True`), never `dumps_http`'s: this is
/// stdout, and finding 11 in the ledger is what a shared writer costs.
#[must_use]
pub fn render_json(value: &Value) -> String {
    stax_memory::pyjson::dumps_pretty(value)
}

/// `render_csv(report)` — the per-project rows, `lineterminator="\n"`.
///
/// `csv.writer` quotes with `QUOTE_MINIMAL`, so a field is quoted only when it
/// contains the delimiter, a quote, or a newline. Project slugs contain none of
/// those (every non-alphanumeric character in a path becomes `-`), but the rule
/// is implemented rather than assumed — a slug is store content, and store
/// content has surprised this campaign before.
#[must_use]
pub fn render_csv(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&csv_row(&["project", "cost", "messages", "sessions"]));
    for row in &report.by_project {
        out.push_str(&csv_row(&[
            &row.name,
            &format!("{:.2}", row.cost),
            // `writer.writerow([… row['messages'] …])` — the ints go in RAW, so
            // there is no `,` grouping here even though the text renderer has it.
            &row.messages.to_string(),
            &row.sessions.to_string(),
        ]));
    }
    out
}

/// One `csv.writer.writerow` line, `QUOTE_MINIMAL`, `lineterminator="\n"`.
#[must_use]
pub fn csv_row(fields: &[&str]) -> String {
    let mut out = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if field.contains([',', '"', '\n', '\r']) {
            out.push('"');
            out.push_str(&field.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(field);
        }
    }
    out.push('\n');
    out
}

/// Python's `f"{n:,}"` — comma every three digits, minus sign outside.
#[must_use]
pub fn py_thousands(value: i64) -> String {
    let negative = value < 0;
    // `unsigned_abs` rather than `abs`: `i64::MIN.abs()` panics, and a message
    // count can only be non-negative in practice — "in practice" is not a proof.
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if negative {
        grouped.push('-');
    }
    let first = digits.len() % 3;
    if first > 0 {
        grouped.push_str(&digits[..first]);
    }
    for (index, chunk) in digits.as_bytes()[first..].chunks(3).enumerate() {
        if index > 0 || first > 0 {
            grouped.push(',');
        }
        grouped.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    grouped
}

/// CPython's `round()` — half to EVEN, returning an integer.
///
/// One call site: [`rich::ratio_reduce`]. See the module docs for why a
/// half-up rounding there moves a whole column.
#[must_use]
pub fn py_round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let diff = value - floor;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "column widths are small integers; the input is a width ratio"
    )]
    let floor_i = floor as i64;
    // Written as one comparison against a boolean rather than the four-arm
    // if/else the reference reads like: clippy rejects that shape (`diff < 0.5`
    // and the even-tie arm return the same expression, which is the POINT of
    // half-to-even and not a copy-paste slip). The arms are, in order:
    // above the tie → up; below the tie → down; exactly on it → to the even
    // neighbour, which is `floor_i` when `floor_i` is already even.
    let round_up = diff > 0.5 || (diff == 0.5 && floor_i % 2 != 0);
    floor_i + i64::from(round_up)
}

/// The Rich subset the three text reports need.
pub mod rich {
    use super::py_round_half_even;

    /// `Column.justify`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Justify {
        /// `justify="left"` — Rich's default.
        Left,
        /// `justify="right"`.
        Right,
    }

    /// `box.HEAVY_HEAD` — the default `Table.box`.
    ///
    /// Rich stores a box as eight lines of four characters; the names below are
    /// `Box.__init__`'s, in its order. `substitute` would swap these for ASCII
    /// on a non-UTF-8 console — the harness pins `PYTHONIOENCODING=utf-8`, and
    /// `Console.encoding` is what Rich actually tests (`ascii_only` is
    /// `not encoding.startswith("utf")`), so the heavy set is the measured one.
    mod heavy_head {
        pub const TOP_LEFT: char = '┏';
        pub const TOP: char = '━';
        pub const TOP_DIVIDER: char = '┳';
        pub const TOP_RIGHT: char = '┓';
        pub const HEAD_LEFT: char = '┃';
        pub const HEAD_VERTICAL: char = '┃';
        pub const HEAD_RIGHT: char = '┃';
        pub const HEAD_ROW_LEFT: char = '┡';
        pub const HEAD_ROW_HORIZONTAL: char = '━';
        pub const HEAD_ROW_CROSS: char = '╇';
        pub const HEAD_ROW_RIGHT: char = '┩';
        pub const MID_LEFT: char = '│';
        pub const MID_VERTICAL: char = '│';
        pub const MID_RIGHT: char = '│';
        pub const BOTTOM_LEFT: char = '└';
        pub const BOTTOM: char = '─';
        pub const BOTTOM_DIVIDER: char = '┴';
        pub const BOTTOM_RIGHT: char = '┘';
    }

    /// `padding=(0, 1)` — one cell each side.
    const PADDING: usize = 2;

    /// The ellipsis `overflow="ellipsis"` appends. U+2026, one cell wide.
    const ELLIPSIS: char = '…';

    /// `Table(show_header=True, header_style="bold")` with the default box and
    /// the default `padding=(0, 1)`.
    #[derive(Debug, Clone)]
    pub struct Table {
        headers: Vec<String>,
        justify: Vec<Justify>,
        rows: Vec<Vec<String>>,
        title: Option<String>,
    }

    impl Table {
        /// A table with the given columns.
        #[must_use]
        pub fn new(columns: &[(&str, Justify)]) -> Self {
            Self {
                headers: columns.iter().map(|(name, _)| (*name).to_owned()).collect(),
                justify: columns.iter().map(|(_, just)| *just).collect(),
                rows: Vec::new(),
                title: None,
            }
        }

        /// `Table(title=…)` — centred above the table, outside the box.
        #[must_use]
        pub fn with_title(mut self, title: impl Into<String>) -> Self {
            self.title = Some(title.into());
            self
        }

        /// `table.add_row(*cells)`.
        pub fn add_row(&mut self, cells: &[String]) {
            self.rows.push(cells.to_vec());
        }

        /// The exact bytes `console.print(table)` writes.
        #[must_use]
        pub fn render(&self, console_width: usize) -> String {
            // `__rich_console__`: the box eats `len(columns) - 1` dividers plus
            // 2 for the edge BEFORE the columns are measured.
            let columns = self.headers.len();
            let budget = console_width.saturating_sub(columns.saturating_sub(1) + 2);

            let natural: Vec<usize> = (0..columns)
                .map(|index| {
                    // `_measure_column` over `Padding(cell, (0,1,0,1))` — the
                    // header is one of the cells, which is why a column of `2`s
                    // under `Sessions` is 10 wide and not 3.
                    let widest = std::iter::once(&self.headers[index])
                        .chain(self.rows.iter().filter_map(|row| row.get(index)))
                        .map(|cell| cell_len(cell))
                        .max()
                        .unwrap_or(0);
                    (widest + PADDING).min(budget.max(1))
                })
                .collect();
            let widths = fit_widths(&natural, budget);

            let mut out = String::new();
            if let Some(title) = &self.title {
                let table_width = widths.iter().sum::<usize>() + columns.saturating_sub(1) + 2;
                out.push_str(&centre(title, table_width));
                out.push('\n');
            }
            out.push_str(&edge(
                &widths,
                heavy_head::TOP_LEFT,
                heavy_head::TOP,
                heavy_head::TOP_DIVIDER,
                heavy_head::TOP_RIGHT,
            ));
            out.push('\n');
            out.push_str(&self.body_row(
                &self.headers,
                &widths,
                heavy_head::HEAD_LEFT,
                heavy_head::HEAD_VERTICAL,
                heavy_head::HEAD_RIGHT,
            ));
            out.push('\n');
            out.push_str(&edge(
                &widths,
                heavy_head::HEAD_ROW_LEFT,
                heavy_head::HEAD_ROW_HORIZONTAL,
                heavy_head::HEAD_ROW_CROSS,
                heavy_head::HEAD_ROW_RIGHT,
            ));
            out.push('\n');
            for row in &self.rows {
                out.push_str(&self.body_row(
                    row,
                    &widths,
                    heavy_head::MID_LEFT,
                    heavy_head::MID_VERTICAL,
                    heavy_head::MID_RIGHT,
                ));
                out.push('\n');
            }
            out.push_str(&edge(
                &widths,
                heavy_head::BOTTOM_LEFT,
                heavy_head::BOTTOM,
                heavy_head::BOTTOM_DIVIDER,
                heavy_head::BOTTOM_RIGHT,
            ));
            out.push('\n');
            out
        }

        fn body_row(
            &self,
            cells: &[String],
            widths: &[usize],
            left: char,
            divider: char,
            right: char,
        ) -> String {
            let empty = String::new();
            let mut line = String::new();
            line.push(left);
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    line.push(divider);
                }
                let cell = cells.get(index).unwrap_or(&empty);
                let justify = self.justify.get(index).copied().unwrap_or(Justify::Left);
                line.push_str(&pad_cell(cell, *width, justify));
            }
            line.push(right);
            line
        }
    }

    /// One horizontal rule: `Box.get_top` / `get_row` / `get_bottom`.
    fn edge(widths: &[usize], left: char, fill: char, divider: char, right: char) -> String {
        let mut line = String::new();
        line.push(left);
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                line.push(divider);
            }
            for _ in 0..*width {
                line.push(fill);
            }
        }
        line.push(right);
        line
    }

    /// One cell, padded and justified into `width` (padding included).
    fn pad_cell(text: &str, width: usize, justify: Justify) -> String {
        // The `Padding` is one cell either side, so the content gets `width - 2`
        // — and a column narrowed below 3 has no content region left at all.
        let content_width = width.saturating_sub(PADDING);
        let truncated = truncate_ellipsis(text, content_width);
        let used = cell_len(&truncated);
        let slack = content_width.saturating_sub(used);
        let mut out = String::with_capacity(width);
        out.push(' ');
        match justify {
            Justify::Left => {
                out.push_str(&truncated);
                out.extend(std::iter::repeat_n(' ', slack));
            }
            Justify::Right => {
                out.extend(std::iter::repeat_n(' ', slack));
                out.push_str(&truncated);
            }
        }
        out.push(' ');
        out
    }

    /// `Text.truncate(width, overflow="ellipsis")`.
    ///
    /// `set_cell_size(plain, max_width - 1) + "…"`. At `max_width == 0` Rich's
    /// `set_cell_size(-1)` would slice to empty and still append the ellipsis;
    /// this returns empty instead, because a zero-width column cannot hold one
    /// and the reference never produces one (the collapse loop stops at the
    /// second-widest column, never at zero).
    #[must_use]
    pub fn truncate_ellipsis(text: &str, max_width: usize) -> String {
        if cell_len(text) <= max_width {
            return text.to_owned();
        }
        if max_width == 0 {
            return String::new();
        }
        let mut out: String = text.chars().take(max_width - 1).collect();
        out.push(ELLIPSIS);
        out
    }

    /// `cell_len` — the printed width of a string.
    ///
    /// Counted in `char`s, which is right for every byte this port emits:
    /// project slugs are ASCII by construction (every non-alphanumeric character
    /// in a path becomes `-`), and the money / count columns are digits. Rich's
    /// own `cell_len` consults an East-Asian width table, so a CJK project
    /// *display name* would render two cells there and one here. No such row
    /// exists in the reference store and inventing a width table to match an
    /// unreachable case would be a second table to keep true — recorded, not
    /// guessed.
    #[must_use]
    pub fn cell_len(text: &str) -> usize {
        text.chars().count()
    }

    /// `Table._calculate_column_widths`' shrink half, with the budget already
    /// reduced by the borders.
    ///
    /// Returns the natural widths untouched when they fit — the reference only
    /// enters `_collapse_widths` when `table_width > max_width`.
    #[must_use]
    pub fn fit_widths(natural: &[usize], budget: usize) -> Vec<usize> {
        let mut widths = natural.to_vec();
        if widths.iter().sum::<usize>() <= budget {
            return widths;
        }
        widths = collapse_widths(&widths, budget);
        // "last resort, reduce columns evenly" — reached only when every column
        // is already at the same width and still too wide.
        let total: usize = widths.iter().sum();
        if total > budget {
            let excess = total - budget;
            let ratios = vec![1_i64; widths.len()];
            let maximums = widths.clone();
            widths = ratio_reduce(excess, &ratios, &maximums, &widths);
        }
        // `_measure_column` runs again against the reduced widths and takes the
        // clamped maximum, i.e. `min(natural, width)`. It matters when the
        // collapse over-shrinks a column past its content.
        widths
            .iter()
            .zip(natural)
            .map(|(width, nat)| (*width).min(*nat))
            .collect()
    }

    /// `Table._collapse_widths` — shrink the widest column toward the
    /// second-widest, repeatedly.
    #[must_use]
    pub fn collapse_widths(widths: &[usize], max_width: usize) -> Vec<usize> {
        let mut widths = widths.to_vec();
        if widths.is_empty() {
            return widths;
        }
        let mut total: usize = widths.iter().sum();
        // Every column here is wrapable: none of the ported tables sets an
        // explicit `width=` or `no_wrap=True`, so `wrapable` is all-true and the
        // per-column guard collapses out of the transcription.
        while total > 0 && total > max_width {
            let excess = total - max_width;
            let max_column = *widths.iter().max().unwrap_or(&0);
            let second_max = widths
                .iter()
                .map(|width| if *width == max_column { 0 } else { *width })
                .max()
                .unwrap_or(0);
            let difference = max_column - second_max;
            let ratios: Vec<i64> = widths
                .iter()
                .map(|width| i64::from(*width == max_column))
                .collect();
            if !ratios.iter().any(|ratio| *ratio != 0) || difference == 0 {
                break;
            }
            let cap = excess.min(difference);
            let maximums = vec![cap; widths.len()];
            widths = ratio_reduce(excess, &ratios, &maximums, &widths);
            total = widths.iter().sum();
        }
        widths
    }

    /// `_ratio.ratio_reduce` — subtract `total` from `values` by `ratios`.
    ///
    /// The `round` is CPython's, hence [`py_round_half_even`]. The
    /// `total_remaining` / `total_ratio` bookkeeping is the reference's: each
    /// column takes its share of what is LEFT, not of the original total, which
    /// is what makes the result sum exactly.
    #[must_use]
    pub fn ratio_reduce(
        total: usize,
        ratios: &[i64],
        maximums: &[usize],
        values: &[usize],
    ) -> Vec<usize> {
        // `ratios = [ratio if _max else 0 …]` — a zero maximum zeroes the ratio.
        let ratios: Vec<i64> = ratios
            .iter()
            .zip(maximums)
            .map(|(ratio, max)| if *max == 0 { 0 } else { *ratio })
            .collect();
        let mut total_ratio: i64 = ratios.iter().sum();
        if total_ratio == 0 {
            return values.to_vec();
        }
        #[allow(
            clippy::cast_possible_wrap,
            reason = "console widths are far below i64::MAX"
        )]
        let mut remaining = total as i64;
        let mut result = Vec::with_capacity(values.len());
        for ((ratio, maximum), value) in ratios.iter().zip(maximums).zip(values) {
            if *ratio != 0 && total_ratio > 0 {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the operands are console widths"
                )]
                let share =
                    py_round_half_even((*ratio as f64) * (remaining as f64) / (total_ratio as f64));
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "console widths are far below i64::MAX"
                )]
                let cap = *maximum as i64;
                let distributed = share.min(cap);
                #[allow(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "the reference does the same arithmetic in Python ints"
                )]
                let reduced = ((*value as i64 - distributed).max(0)) as usize;
                result.push(reduced);
                remaining -= distributed;
                total_ratio -= *ratio;
            } else {
                result.push(*value);
            }
        }
        result
    }

    /// `console.print(text)` — markup stripped, word-wrapped at `width`.
    ///
    /// Wrapping is `_wrap.divide_line` with `fold=True` for the one case that
    /// can reach it (a line longer than the console). Every line is
    /// right-stripped, which is why a short line carries no trailing spaces.
    #[must_use]
    pub fn print_text(markup: &str, width: usize) -> String {
        let plain = strip_markup(markup);
        let mut out = String::new();
        for line in wrap(&plain, width.max(1)) {
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// `rich.markup.render` with every style dropped.
    ///
    /// Handles the closed `[tag]` / `[/tag]` shape `render.py` uses and nothing
    /// more — see the module docs for why the general case is a recorded
    /// divergence rather than a guess.
    #[must_use]
    pub fn strip_markup(markup: &str) -> String {
        let mut out = String::new();
        let mut rest = markup;
        while let Some(open) = rest.find('[') {
            let Some(offset) = rest[open..].find(']') else {
                break;
            };
            let close = open + offset;
            let tag = &rest[open + 1..close];
            // Rich renders `[]` and a tag containing `[` as literal text.
            if tag.is_empty() || tag.contains('[') {
                out.push_str(&rest[..=close]);
                rest = &rest[close + 1..];
                continue;
            }
            out.push_str(&rest[..open]);
            rest = &rest[close + 1..];
        }
        out.push_str(rest);
        out
    }

    /// `Text.wrap` for a single-line string: `divide_line(text, width, fold=True)`.
    #[must_use]
    pub fn wrap(text: &str, width: usize) -> Vec<String> {
        if cell_len(text) <= width {
            return vec![text.to_owned()];
        }
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut position = 0_usize;
        for word in words(text) {
            let word_length = cell_len(word.trim_end());
            if position + word_length > width {
                if word_length > width {
                    if position > 0 {
                        lines.push(std::mem::take(&mut current));
                        position = 0;
                    }
                    // `chop_cells` — hard-fold the over-long word.
                    let chars: Vec<char> = word.chars().collect();
                    for chunk in chars.chunks(width) {
                        let piece: String = chunk.iter().collect();
                        position = cell_len(&piece);
                        current = piece;
                        if position == width {
                            lines.push(std::mem::take(&mut current));
                            position = 0;
                        }
                    }
                } else if position > 0 {
                    lines.push(std::mem::take(&mut current));
                    current.push_str(word);
                    position = cell_len(word);
                } else {
                    current.push_str(word);
                    position = cell_len(word);
                }
            } else {
                current.push_str(word);
                position += cell_len(word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    /// `_wrap.words` — each word carries the whitespace that follows it.
    fn words(text: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut start = 0_usize;
        let mut index = 0_usize;
        while index < chars.len() {
            while index < chars.len() && !chars[index].1.is_whitespace() {
                index += 1;
            }
            while index < chars.len() && chars[index].1.is_whitespace() {
                index += 1;
            }
            let end = chars.get(index).map_or(text.len(), |(offset, _)| *offset);
            if end > start {
                out.push(&text[start..end]);
            }
            start = end;
        }
        out
    }

    /// Centre `text` in `width` — the table TITLE row, padded on **both** sides.
    ///
    /// # DIV-373 — this used to pad only the left, and nothing crossed it
    ///
    /// The title is rendered by `Table.__rich_console__` as a `Text` with
    /// `justify="center"` sized to the *table's* width (not the console's), and
    /// `Text.__rich_console__` emits `pad_left` **and** `pad_right` segments,
    /// which the console writes verbatim — a title row is `width` cells wide
    /// with real trailing spaces. That is unlike a bare `console.print("…")`,
    /// which right-strips ([`print_text`]).
    ///
    /// DIV-270 proved the layout engine on `report -p all` — 22,001 bytes,
    /// byte-identical — and `report` passes **no title**, so this branch shipped
    /// unexercised and left-padding-only. `stax compare` is the first caller
    /// with a title and it failed on exactly this: 95 bytes vs 55, the whole
    /// difference being the 40 trailing spaces. Wave 6's law again, on a third
    /// crate: *a constant a port copies needs a row that crosses it* — and the
    /// generalisation this time is that an OPTION nothing sets is the same
    /// thing as a constant nothing crosses.
    fn centre(text: &str, width: usize) -> String {
        let used = cell_len(text);
        if used >= width {
            return text.to_owned();
        }
        // `(width - used) // 2` on the left, the REMAINDER on the right — an
        // odd slack puts the extra cell on the right, as `Text.pad_right` does.
        let left = (width - used) / 2;
        let right = width - used - left;
        let mut out = String::with_capacity(width);
        out.extend(std::iter::repeat_n(' ', left));
        out.push_str(text);
        out.extend(std::iter::repeat_n(' ', right));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::rich::{
        Justify, Table, cell_len, fit_widths, strip_markup, truncate_ellipsis, wrap,
    };
    use super::{csv_row, py_round_half_even, py_thousands, render_csv, render_text};
    use crate::aggregate::{ProjectRow, Report};

    fn report(label: &str, rows: Vec<ProjectRow>) -> Report {
        Report {
            scope_label: label.to_owned(),
            // `0.0 + …`, not `.sum()`: Rust's `Sum for f64` folds from `-0.0`,
            // so an empty report would render `$-0.00` here where Python's
            // `sum([])` is an int `0`. A fixture that lies about the reference is
            // worse than no fixture.
            total_cost: rows.iter().fold(0.0, |acc, row| acc + row.cost),
            total_messages: rows.iter().map(|row| row.messages).sum(),
            total_sessions: rows.iter().map(|row| row.sessions).sum(),
            by_project: rows,
        }
    }

    fn row(name: &str, cost: f64, messages: i64, sessions: i64) -> ProjectRow {
        ProjectRow {
            name: name.to_owned(),
            cost,
            messages,
            sessions,
        }
    }

    #[test]
    fn an_empty_report_prints_three_lines_and_no_table() {
        // Recorded from `stackunderflow today` on the parity `fresh` state.
        assert_eq!(
            render_text(&report("today", Vec::new()), 80),
            "StackUnderflow — today\nNo activity in this period.\nTotal: $0.00  0 messages  0 sessions\n"
        );
    }

    #[test]
    fn a_narrow_table_is_riches_heavy_head_box() {
        // Recorded from `rich.table.Table` at COLUMNS=80.
        let out = render_text(&report("all time", vec![row("a", 1.0, 5, 2)]), 80);
        assert_eq!(
            out,
            concat!(
                "StackUnderflow — all time\n",
                "┏━━━━━━━━━┳━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓\n",
                "┃ Project ┃  Cost ┃ Messages ┃ Sessions ┃\n",
                "┡━━━━━━━━━╇━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩\n",
                "│ a       │ $1.00 │        5 │        2 │\n",
                "└─────────┴───────┴──────────┴──────────┘\n",
                "Total: $1.00  5 messages  2 sessions\n",
            )
        );
    }

    #[test]
    fn the_header_widens_a_column_its_cells_do_not() {
        // `Sessions` is 8 cells and every cell under it is 1 — the column is 10
        // (8 + padding), which only holds if `_measure_column` measures the
        // header cell. It does; a port that measured only the rows would be
        // seven characters narrow and shift the whole box.
        let out = render_text(&report("x", vec![row("a", 1.0, 5, 2)]), 80);
        let header = out.lines().nth(2).expect("header row");
        assert!(header.ends_with("┃ Sessions ┃"), "{header}");
    }

    #[test]
    fn an_over_wide_table_shrinks_only_the_widest_column() {
        // Recorded from rich: natural widths [72, 11, 10, 10], budget 75, and
        // the widest column alone absorbs all 28 cells of excess.
        assert_eq!(fit_widths(&[72, 11, 10, 10], 75), vec![44, 11, 10, 10]);
    }

    #[test]
    fn two_equally_wide_columns_shrink_together_and_the_round_is_bankers() {
        // Recorded from rich: [62, 7, 32, 10] at budget 75 collapses in two
        // passes — the first drags 62 down to 32 (capped by the difference to
        // the second-widest), the second takes 3 from each of the two 32s.
        assert_eq!(fit_widths(&[62, 7, 32, 10], 75), vec![29, 7, 29, 10]);
    }

    #[test]
    fn a_shrunken_cell_is_truncated_with_one_ellipsis() {
        let out = render_text(
            &report("all time", vec![row(&"y".repeat(60), 1.0, 5, 2)]),
            80,
        );
        let data = out.lines().nth(4).expect("data row");
        assert_eq!(cell_len(data), 80, "the box fills the console exactly");
        assert!(data.contains(&format!("{}…", "y".repeat(41))), "{data}");
    }

    #[test]
    fn the_rounding_is_half_to_even_not_half_up() {
        assert_eq!(py_round_half_even(0.5), 0);
        assert_eq!(py_round_half_even(1.5), 2);
        assert_eq!(py_round_half_even(2.5), 2);
        assert_eq!(py_round_half_even(3.5), 4);
        assert_eq!(py_round_half_even(2.4), 2);
        assert_eq!(py_round_half_even(2.6), 3);
    }

    #[test]
    fn thousands_separators_match_pythons_format_spec() {
        assert_eq!(py_thousands(0), "0");
        assert_eq!(py_thousands(999), "999");
        assert_eq!(py_thousands(1_000), "1,000");
        assert_eq!(py_thousands(30_934), "30,934");
        assert_eq!(py_thousands(1_234_567), "1,234,567");
        assert_eq!(py_thousands(-1_234), "-1,234");
        assert_eq!(py_thousands(i64::MIN), "-9,223,372,036,854,775,808");
    }

    #[test]
    fn markup_tags_are_dropped_and_their_text_is_kept() {
        assert_eq!(strip_markup("[bold]Total:[/bold] $1.00"), "Total: $1.00");
        assert_eq!(
            strip_markup("[dim]No activity in this period.[/dim]"),
            "No activity in this period."
        );
        assert_eq!(strip_markup("no markup here"), "no markup here");
        // Rich renders `[]` literally; so does this.
        assert_eq!(strip_markup("a [] b"), "a [] b");
    }

    #[test]
    fn truncation_counts_characters_and_the_ellipsis_replaces_one() {
        assert_eq!(truncate_ellipsis("abcdef", 6), "abcdef");
        assert_eq!(truncate_ellipsis("abcdef", 5), "abcd…");
        assert_eq!(truncate_ellipsis("abcdef", 1), "…");
        assert_eq!(truncate_ellipsis("abcdef", 0), "");
    }

    #[test]
    fn a_line_that_fits_is_not_wrapped_and_carries_no_trailing_space() {
        assert_eq!(wrap("Total: $1.00", 80), vec!["Total: $1.00".to_owned()]);
        assert_eq!(
            super::rich::print_text("[bold]Total:[/bold] $1.00", 80),
            "Total: $1.00\n"
        );
    }

    #[test]
    fn the_csv_writer_is_quote_minimal_and_the_counts_are_ungrouped() {
        let out = render_csv(&report("x", vec![row("-a-b", 12.345, 30_934, 160)]));
        assert_eq!(
            out, "project,cost,messages,sessions\n-a-b,12.35,30934,160\n",
            "`:.2f` on the cost, raw ints on the counts"
        );
        // The quoting rule is implemented, not assumed absent.
        assert_eq!(csv_row(&["a,b", "c\"d"]), "\"a,b\",\"c\"\"d\"\n");
    }

    #[test]
    fn a_table_that_exactly_fills_the_console_is_not_shrunk() {
        // 42 + 7 + 10 + 10 = 69 natural, budget 75 — no collapse at all.
        let out = render_text(
            &report("all time", vec![row(&"x".repeat(40), 1.0, 5, 2)]),
            80,
        );
        let top = out.lines().nth(1).expect("top rule");
        // 42 + 7 + 10 + 10 columns, 3 dividers, 2 edges.
        assert_eq!(cell_len(top), 74);
        assert!(!out.contains('…'), "nothing was truncated");
    }

    #[test]
    fn a_title_longer_than_its_table_is_emitted_unpadded() {
        let mut table = Table::new(&[("A", Justify::Left)]).with_title("Compare — month");
        table.add_row(&["x".to_owned()]);
        let rendered = table.render(80);
        assert!(rendered.starts_with("Compare — month\n"), "{rendered}");
    }
}
