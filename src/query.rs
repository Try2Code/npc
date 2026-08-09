//! The query language, compiled to SQL.
//!
//! One parser backs `photo find`, the bulk subcommands and the TUI's `/`, so
//! the syntax is learned once. Barewords are tag names and adjacency implies
//! AND, which makes the common case short:
//!
//! ```text
//! beach anna rating>=4
//! beach and (anna or ben) and not private
//! year=2026 camera~D750 untagged
//! date=2026-08-01..2026-08-09 iso>1600
//! flagged and not rejected
//! ```
//!
//! Values are always bound as parameters. A tag containing a quote or a
//! semicolon is data, never syntax -- see the injection test at the bottom.

use anyhow::{anyhow, bail, Result};
use rusqlite::types::Value;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Not,
    /// key, operator, value
    Pred(String, Op, String),
    Word(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Like,
}

impl Op {
    fn sql(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Like => "LIKE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Compiled {
    /// A boolean expression over the alias `s` (the `stack` table).
    pub where_sql: String,
    pub params: Vec<Value>,
}

impl Compiled {
    /// Full statement selecting matching stacks, newest capture first.
    pub fn select_stacks(&self, limit: Option<usize>) -> String {
        let lim = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
        format!(
            "SELECT s.id, s.dir, s.base, s.rating, s.flag, s.reject,
                    (SELECT MIN(f.capture_time) FROM file f
                      WHERE f.stack_id = s.id AND f.capture_time IS NOT NULL) AS captured
             FROM stack s
             WHERE {}
             ORDER BY captured IS NULL, captured DESC, s.dir, s.base{lim}",
            self.where_sql
        )
    }
}

/// The reference, printed by `photo find --help-syntax`. Kept here beside the
/// parser so the two cannot drift: every key listed below is one the parser
/// actually accepts.
pub const SYNTAX: &str = "\
QUERY SYNTAX

  Barewords are tag names, and adjacency means AND:

    beach                       tagged `beach`
    beach anna                  both tags
    beach and anna              the same thing, written out
    beach or anna               either
    beach and (anna or ben)     grouping
    not private                 negation
    -private                    the same, shorthand
    \"family trip\"               quote anything containing spaces

  Tags are hierarchical, so a parent selects everything beneath it:
  `beach` also matches `beach/sunset`.

KEYS

  rating       rating>=4   rating=0   rating!=5
  year         year=2026   year=2024..2026
  date         date=2026-08-09        date=2026-08-01..2026-08-09
  camera       camera~D750            camera=\"NIKON D750\"
  lens         lens~80-200
  serial       serial=6094708
  iso          iso>1600    iso<=400
  shutter      shutter>50000          (shutter actuation count)
  width height width>=6000
  ext          ext=NEF     ext=jpg    (case-insensitive)
  role         role=raw    role=oocjpg  role=derived  role=profile
  path         path~08/09             (substring of dir/base)
  name         name~750_69
  title        title~sunset
  description  description~\"low tide\"
  tag          tag:beach              (explicit form of a bareword)

FLAGS  (no value)

  tagged  untagged      has any tag / none
  rated   unrated       has a star rating / none
  flagged rejected      the pick and reject marks
  offline               no member currently present on disk

OPERATORS

  =  !=  >  >=  <  <=      comparison
  ~                        substring match
  ..                       inclusive range, for year and date

EXAMPLES

  photo find 'rating>=4'
  photo find 'beach rating>=4 -private'
  photo find 'year=2026 camera~D750 untagged'
  photo find 'date=2026-08-01..2026-08-09 iso>1600'
  photo find 'flagged and not rejected'
  photo find 'rating=5 or flagged' --files --print0 | xargs -0 ls -l

  Values are always bound as parameters, so a tag containing quotes or
  semicolons is data, never syntax.
";

// --- tokenising ----------------------------------------------------------

fn tokenize(input: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '(' {
            out.push(Tok::LParen);
            i += 1;
            continue;
        }
        if c == ')' {
            out.push(Tok::RParen);
            i += 1;
            continue;
        }
        // `-tag` is shorthand for `not tag`, but only leading and only when it
        // starts a term -- a hyphen inside a word is part of the name.
        if c == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
            let starts_term = out
                .last()
                .map(|t| matches!(t, Tok::LParen | Tok::And | Tok::Or | Tok::Not))
                .unwrap_or(true);
            if starts_term {
                out.push(Tok::Not);
                i += 1;
                continue;
            }
        }

        // A bare word, a quoted string, or key<op>value.
        let (word, next) = read_word(&chars, i)?;
        i = next;

        match word.to_ascii_lowercase().as_str() {
            "and" => out.push(Tok::And),
            "or" => out.push(Tok::Or),
            "not" => out.push(Tok::Not),
            _ => out.push(split_predicate(&word)),
        }
    }

    Ok(out)
}

/// Read one token's text, honouring quotes so tags may contain spaces.
fn read_word(chars: &[char], start: usize) -> Result<(String, usize)> {
    let mut i = start;
    let mut s = String::new();

    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                s.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                bail!("unterminated {quote} in query");
            }
            i += 1; // closing quote
            continue;
        }
        if c.is_whitespace() || c == '(' || c == ')' {
            break;
        }
        s.push(c);
        i += 1;
    }

    if s.is_empty() {
        bail!("empty token in query");
    }
    Ok((s, i))
}

/// Split `key>=value` into its parts. Anything without an operator is a tag.
fn split_predicate(word: &str) -> Tok {
    const OPS: &[(&str, Op)] = &[
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("!=", Op::Ne),
        ("~", Op::Like),
        ("=", Op::Eq),
        (">", Op::Gt),
        ("<", Op::Lt),
        (":", Op::Eq),
    ];
    for (sym, op) in OPS {
        if let Some(idx) = word.find(sym) {
            if idx > 0 {
                let key = word[..idx].to_ascii_lowercase();
                let value = word[idx + sym.len()..].to_string();
                if !value.is_empty() {
                    return Tok::Pred(key, *op, value);
                }
            }
        }
    }
    Tok::Word(word.to_string())
}

// --- parsing -------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    params: Vec<Value>,
}

pub fn compile(query: &str) -> Result<Compiled> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Compiled {
            where_sql: "1".into(),
            params: Vec::new(),
        });
    }

    let toks = tokenize(trimmed)?;
    let mut p = Parser {
        toks,
        pos: 0,
        params: Vec::new(),
    };
    let sql = p.parse_or()?;
    if p.pos != p.toks.len() {
        bail!("unexpected `)` or trailing input in query");
    }
    Ok(Compiled {
        where_sql: sql,
        params: p.params,
    })
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bind(&mut self, v: Value) -> String {
        self.params.push(v);
        format!("?{}", self.params.len())
    }

    fn parse_or(&mut self) -> Result<String> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Tok::Or)) {
            self.pos += 1;
            let right = self.parse_and()?;
            left = format!("({left} OR {right})");
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<String> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.pos += 1;
                }
                // Adjacency implies AND: `beach anna` == `beach and anna`.
                Some(Tok::Word(_)) | Some(Tok::Pred(..)) | Some(Tok::LParen) | Some(Tok::Not) => {}
                _ => break,
            }
            let right = self.parse_unary()?;
            left = format!("({left} AND {right})");
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<String> {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.pos += 1;
            let inner = self.parse_unary()?;
            return Ok(format!("(NOT {inner})"));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Tok::LParen) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    bail!("missing `)` in query");
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(Tok::Word(w)) => {
                self.pos += 1;
                self.bare_word(&w)
            }
            Some(Tok::Pred(k, op, v)) => {
                self.pos += 1;
                self.predicate(&k, op, &v)
            }
            Some(t) => Err(anyhow!("unexpected {t:?} in query")),
            None => Err(anyhow!("query ended unexpectedly")),
        }
    }

    /// A bareword is a flag if it names one, otherwise a tag.
    fn bare_word(&mut self, w: &str) -> Result<String> {
        Ok(match w.to_ascii_lowercase().as_str() {
            "untagged" => "NOT EXISTS (SELECT 1 FROM stack_tag st WHERE st.stack_id = s.id)".into(),
            "tagged" => "EXISTS (SELECT 1 FROM stack_tag st WHERE st.stack_id = s.id)".into(),
            "flagged" => "s.flag = 1".into(),
            "rejected" => "s.reject = 1".into(),
            "rated" => "s.rating IS NOT NULL AND s.rating > 0".into(),
            "unrated" => "(s.rating IS NULL OR s.rating = 0)".into(),
            "offline" => "NOT EXISTS (SELECT 1 FROM file f WHERE f.stack_id = s.id AND f.present = 1)".into(),
            _ => self.tag_match(w),
        })
    }

    /// Tags are hierarchical: `beach` matches `beach` and `beach/sunset`, so a
    /// parent selects everything under it.
    fn tag_match(&mut self, name: &str) -> String {
        let exact = self.bind(Value::Text(name.to_string()));
        let child = self.bind(Value::Text(format!("{name}/%")));
        format!(
            "EXISTS (SELECT 1 FROM stack_tag st JOIN tag t ON t.id = st.tag_id
                     WHERE st.stack_id = s.id AND (t.name = {exact} OR t.name LIKE {child}))"
        )
    }

    fn predicate(&mut self, key: &str, op: Op, value: &str) -> Result<String> {
        match key {
            "tag" => Ok(self.tag_match(value)),

            "rating" => {
                let n: i64 = value.parse().map_err(|_| anyhow!("rating must be a number, got `{value}`"))?;
                let p = self.bind(Value::Integer(n));
                Ok(format!("COALESCE(s.rating, 0) {} {p}", op.sql()))
            }

            "year" => {
                if let Some((a, b)) = value.split_once("..") {
                    let (lo, _) = day_bounds(&format!("{a}-01-01"))?;
                    let (_, hi) = day_bounds(&format!("{b}-12-31"))?;
                    Ok(self.capture_between(lo, hi))
                } else {
                    let (lo, _) = day_bounds(&format!("{value}-01-01"))?;
                    let (_, hi) = day_bounds(&format!("{value}-12-31"))?;
                    Ok(self.capture_between(lo, hi))
                }
            }

            "date" => {
                let (lo, hi) = if let Some((a, b)) = value.split_once("..") {
                    (day_bounds(a)?.0, day_bounds(b)?.1)
                } else {
                    day_bounds(value)?
                };
                Ok(self.capture_between(lo, hi))
            }

            "camera" | "lens" | "serial" => Ok(self.file_text(key, op, value)),
            "ext" => Ok(self.file_text("ext", op, value)),
            "role" => Ok(self.file_text("role", op, value)),

            "iso" | "shutter" | "width" | "height" => {
                let column = if key == "shutter" { "shutter_count" } else { key };
                let n: i64 = value
                    .parse()
                    .map_err(|_| anyhow!("{key} must be a number, got `{value}`"))?;
                let p = self.bind(Value::Integer(n));
                Ok(format!(
                    "EXISTS (SELECT 1 FROM file f WHERE f.stack_id = s.id AND f.{column} {} {p})",
                    op.sql()
                ))
            }

            "path" => {
                let p = self.bind(Value::Text(format!("%{value}%")));
                Ok(format!("(s.dir || '/' || s.base) LIKE {p}"))
            }
            "name" => {
                let p = self.bind(Value::Text(format!("%{value}%")));
                Ok(format!("s.base LIKE {p}"))
            }

            "title" | "description" => {
                let column = key;
                if op == Op::Like {
                    let p = self.bind(Value::Text(format!("%{value}%")));
                    Ok(format!("COALESCE(s.{column},'') LIKE {p}"))
                } else {
                    let p = self.bind(Value::Text(value.to_string()));
                    Ok(format!("COALESCE(s.{column},'') {} {p}", op.sql()))
                }
            }

            other => Err(anyhow!(
                "unknown key `{other}`. Known: tag rating year date camera lens serial \
                 ext role iso shutter width height path name title description"
            )),
        }
    }

    fn capture_between(&mut self, lo: i64, hi: i64) -> String {
        let a = self.bind(Value::Integer(lo));
        let b = self.bind(Value::Integer(hi));
        format!(
            "EXISTS (SELECT 1 FROM file f WHERE f.stack_id = s.id
                     AND f.capture_time BETWEEN {a} AND {b})"
        )
    }

    fn file_text(&mut self, column: &str, op: Op, value: &str) -> String {
        if op == Op::Like {
            let p = self.bind(Value::Text(format!("%{value}%")));
            format!(
                "EXISTS (SELECT 1 FROM file f WHERE f.stack_id = s.id
                         AND COALESCE(f.{column},'') LIKE {p})"
            )
        } else {
            let p = self.bind(Value::Text(value.to_string()));
            format!(
                "EXISTS (SELECT 1 FROM file f WHERE f.stack_id = s.id
                         AND COALESCE(f.{column},'') {} {p} COLLATE NOCASE)",
                op.sql()
            )
        }
    }
}

/// `YYYY-MM-DD` (or `YYYY-MM`, `YYYY`) to the unix range covering it. EXIF
/// times are stored as if UTC, so these bounds use the same convention.
fn day_bounds(s: &str) -> Result<(i64, i64)> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    let year: i64 = parts
        .first()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| anyhow!("bad date `{s}`"))?;
    let month: i64 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(1);
    let day: i64 = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(1);

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        bail!("bad date `{s}`");
    }

    let start = days_from_civil(year, month, day) * 86_400;
    let end = match parts.len() {
        1 => days_from_civil(year + 1, 1, 1) * 86_400 - 1,
        2 => {
            let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
            days_from_civil(ny, nm, 1) * 86_400 - 1
        }
        _ => start + 86_399,
    };
    Ok((start, end))
}

/// Public wrapper so `import` can assert its inverse round-trips.
pub fn days_from_civil_pub(y: i64, m: i64, d: i64) -> i64 { days_from_civil(y, m, d) }

/// Howard Hinnant's civil-to-days algorithm; avoids a chrono dependency.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(q: &str) -> String {
        compile(q).unwrap().where_sql
    }
    fn params(q: &str) -> Vec<Value> {
        compile(q).unwrap().params
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(sql(""), "1");
        assert_eq!(sql("   "), "1");
    }

    #[test]
    fn adjacency_implies_and() {
        let a = sql("beach anna");
        let b = sql("beach and anna");
        assert_eq!(a, b);
        assert!(a.contains(" AND "));
    }

    #[test]
    fn or_binds_looser_than_and() {
        // `a and b or c` must parse as `(a and b) or c`
        let s = sql("a b or c");
        assert!(s.starts_with("((") && s.contains(" AND ") && s.contains(" OR "));
    }

    #[test]
    fn parentheses_override_precedence() {
        let s = sql("beach and (anna or ben)");
        let or_at = s.find(" OR ").unwrap();
        let and_at = s.find(" AND ").unwrap();
        assert!(and_at < or_at, "grouping was lost: {s}");
    }

    #[test]
    fn leading_hyphen_negates_but_inner_hyphen_does_not() {
        assert!(sql("-private").contains("NOT"));
        let p = params("well-known");
        assert_eq!(p[0], Value::Text("well-known".into()));
    }

    #[test]
    fn not_keyword_works() {
        assert!(sql("not rejected").contains("NOT"));
        assert!(sql("flagged and not rejected").contains("NOT"));
    }

    #[test]
    fn flags_compile_without_parameters() {
        for flag in ["untagged", "tagged", "flagged", "rejected", "rated", "unrated"] {
            assert!(params(flag).is_empty(), "{flag} should bind nothing");
            assert!(!sql(flag).is_empty());
        }
    }

    #[test]
    fn tags_match_hierarchically() {
        let p = params("beach");
        assert_eq!(p[0], Value::Text("beach".into()));
        assert_eq!(p[1], Value::Text("beach/%".into()));
    }

    #[test]
    fn rating_comparisons() {
        assert!(sql("rating>=4").contains(">="));
        assert!(sql("rating=0").contains("="));
        assert_eq!(params("rating>=4")[0], Value::Integer(4));
        assert!(compile("rating>=x").is_err());
    }

    #[test]
    fn year_and_date_become_capture_ranges() {
        let p = params("year=2026");
        assert_eq!(p.len(), 2);
        let (lo, hi) = match (&p[0], &p[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!("expected integers"),
        };
        assert!(lo < hi);
        // 2026-01-01T00:00:00Z .. 2026-12-31T23:59:59Z
        assert_eq!(lo, 1_767_225_600);
        assert_eq!(hi, 1_798_761_599);

        let d = params("date=2026-08-09");
        assert_eq!(d.len(), 2);
        match (&d[0], &d[1]) {
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(b - a, 86_399),
            _ => panic!(),
        }
    }

    #[test]
    fn date_ranges_span_both_ends() {
        let p = params("date=2026-08-01..2026-08-09");
        match (&p[0], &p[1]) {
            (Value::Integer(a), Value::Integer(b)) => {
                assert_eq!(*b - *a, 9 * 86_400 - 1);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn civil_date_conversion_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2026, 8, 9) * 86400, 1_786_233_600);
    }

    #[test]
    fn tilde_is_substring_match() {
        let s = sql("camera~D750");
        assert!(s.contains("LIKE"));
        assert_eq!(params("camera~D750")[0], Value::Text("%D750%".into()));
    }

    #[test]
    fn quoted_values_may_contain_spaces() {
        let p = params(r#"tag:"family trip""#);
        assert_eq!(p[0], Value::Text("family trip".into()));
    }

    #[test]
    fn unknown_keys_are_rejected_with_a_useful_message() {
        let err = compile("colour=red").unwrap_err().to_string();
        assert!(err.contains("unknown key"));
        assert!(err.contains("rating"), "should list the known keys: {err}");
    }

    #[test]
    fn malformed_queries_error_rather_than_silently_matching() {
        assert!(compile("(beach").is_err());
        assert!(compile("beach)").is_err());
        assert!(compile("beach and").is_err());
        assert!(compile(r#"tag:"unterminated"#).is_err());
    }

    /// Values reach SQLite as bound parameters, never as text spliced into the
    /// statement. A tag is data even when it looks like SQL.
    #[test]
    fn values_are_never_interpolated_into_sql() {
        let hostile = r#"'; DROP TABLE stack; --"#;
        let c = compile(&format!("tag:\"{hostile}\"")).unwrap();

        assert!(
            !c.where_sql.contains("DROP"),
            "the value leaked into the SQL text: {}",
            c.where_sql
        );
        assert!(c.where_sql.contains("?1"));
        assert_eq!(c.params[0], Value::Text(hostile.to_string()));
    }

    /// Documentation drift is what produced `--help-syntax` being advertised
    /// before it existed. Every key and flag the reference lists must actually
    /// parse, and every key the parser knows must be documented -- both
    /// directions, so neither side can quietly get ahead of the other.
    #[test]
    fn the_syntax_reference_matches_what_the_parser_accepts() {
        const KEYS: &[(&str, &str)] = &[
            ("rating", "rating>=4"),
            ("year", "year=2026"),
            ("date", "date=2026-08-09"),
            ("camera", "camera~D750"),
            ("lens", "lens~80-200"),
            ("serial", "serial=6094708"),
            ("iso", "iso>1600"),
            ("shutter", "shutter>50000"),
            ("width", "width>=6000"),
            ("height", "height>=4000"),
            ("ext", "ext=NEF"),
            ("role", "role=raw"),
            ("path", "path~08/09"),
            ("name", "name~750"),
            ("title", "title~sunset"),
            ("description", "description~tide"),
            ("tag", "tag:beach"),
        ];
        const FLAGS: &[&str] = &[
            "tagged", "untagged", "rated", "unrated", "flagged", "rejected", "offline",
        ];

        for (key, example) in KEYS {
            compile(example)
                .unwrap_or_else(|e| panic!("documented key `{key}` does not parse: {e}"));
            assert!(
                SYNTAX.contains(key),
                "`{key}` is accepted but missing from the reference"
            );
        }
        for flag in FLAGS {
            compile(flag).unwrap_or_else(|e| panic!("documented flag `{flag}` does not parse: {e}"));
            assert!(
                SYNTAX.contains(flag),
                "`{flag}` is accepted but missing from the reference"
            );
        }

        // ..and the worked examples must all compile.
        for line in SYNTAX.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("photo find '") {
                let q = &rest[..rest.find('\'').expect("unterminated example")];
                compile(q).unwrap_or_else(|e| panic!("example `{q}` does not parse: {e}"));
            }
        }
    }

    #[test]
    fn generated_sql_is_accepted_by_sqlite() {
        let cat = crate::db::Catalog::open_memory().unwrap();
        for q in [
            "",
            "beach",
            "beach anna",
            "beach and (anna or ben)",
            "-private",
            "not rejected",
            "rating>=4",
            "year=2026",
            "date=2026-08-01..2026-08-09",
            "camera~D750",
            "lens~Nikkor",
            "iso>1600",
            "ext=NEF",
            "untagged",
            "flagged and not rejected",
            "path~08/09",
            "title~sunset",
            "rating>=3 year=2026 camera~D750 -private",
        ] {
            let c = compile(q).unwrap_or_else(|e| panic!("compiling `{q}`: {e}"));
            let stmt = c.select_stacks(Some(5));
            let mut prepared = cat
                .conn
                .prepare(&stmt)
                .unwrap_or_else(|e| panic!("SQLite rejected `{q}`:\n{stmt}\n{e}"));
            let params: Vec<&dyn rusqlite::ToSql> =
                c.params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            let rows = prepared
                .query_map(params.as_slice(), |_| Ok(()))
                .unwrap_or_else(|e| panic!("executing `{q}`: {e}"));
            assert_eq!(rows.count(), 0, "empty catalog should match nothing");
        }
    }
}
