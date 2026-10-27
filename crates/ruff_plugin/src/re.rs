//! The `re` module of plugins: regular expressions, with an API modeled on Python's.
//!
//! Patterns use the syntax of Rust's `regex` crate, which is close to Python's but doesn't
//! support look-around or backreferences. Positions count characters, as in Python.

// The functions of a `#[starlark_module]` must return `Result`s.
#![expect(clippy::unnecessary_wraps)]

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use allocative::Allocative;
use regex::{Captures, Regex};
use rustc_hash::FxHashMap;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::list::AllocList;
use starlark::values::none::NoneOr;
use starlark::values::tuple::AllocTuple;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, starlark_value};

/// Compiles a pattern, reusing the compiled expression of a pattern compiled before on this
/// thread. The expression is shared, rather than cloned, because a clone of a [`Regex`] starts
/// with empty caches for matching.
pub(crate) fn compile(pattern: &str) -> anyhow::Result<Rc<Regex>> {
    thread_local! {
        static CACHE: RefCell<FxHashMap<String, Rc<Regex>>> = RefCell::new(FxHashMap::default());
    }
    CACHE.with_borrow_mut(|cache| {
        if let Some(regex) = cache.get(pattern) {
            return Ok(regex.clone());
        }
        let regex = Rc::new(
            Regex::new(pattern)
                .map_err(|err| anyhow::anyhow!("Invalid regular expression `{pattern}`: {err}"))?,
        );
        // Patterns are almost always literals in a plugin's source, so the cache stays small;
        // this only guards against patterns built from the code being linted.
        if cache.len() > 1000 {
            cache.clear();
        }
        cache.insert(pattern.to_string(), regex.clone());
        Ok(regex)
    })
}

/// A pattern that matches all of a string if `pattern` does. `pattern` must be valid on its own,
/// so that it can't close the group around it (as `a)|(b` would).
fn fully_anchored(pattern: &str) -> anyhow::Result<String> {
    compile(pattern)?;
    // In verbose mode, a comment at the end of the pattern would hide the end of the group, so a
    // newline ends it. Outside verbose mode, a newline would have to match.
    let end = if enables_verbose_mode(pattern) {
        "\n"
    } else {
        ""
    };
    Ok(format!("\\A(?:{pattern}{end})\\z"))
}

/// Whether a pattern sets the `x` flag, as in `(?x)` or `(?ix:...)`.
fn enables_verbose_mode(pattern: &str) -> bool {
    pattern.match_indices("(?").any(|(index, _)| {
        let rest = &pattern[index + 2..];
        let flags = rest
            .find(|char: char| !char.is_ascii_alphabetic() && char != '-')
            .map_or(rest, |end| &rest[..end]);
        let enabled = flags.split('-').next().unwrap_or_default();
        enabled.contains('x') && rest[flags.len()..].starts_with([':', ')'])
    })
}

/// The `re` global.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ReModule;

starlark_simple_value!(ReModule);

impl fmt::Display for ReModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<module re>")
    }
}

// `starlark_value` requires a named lifetime.
#[expect(clippy::elidable_lifetime_names)]
#[starlark_value(type = "module")]
impl<'v> StarlarkValue<'v> for ReModule {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new();
        METHODS.methods(re_methods)
    }
}

/// A successful match, like Python's `re.Match`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct MatchValue {
    /// Each group's text and character offsets, starting with the whole match.
    groups: Vec<Option<(String, usize, usize)>>,
}

starlark_simple_value!(MatchValue);

impl MatchValue {
    fn new(text: &str, captures: &Captures) -> Self {
        let chars = |byte: usize| text[..byte].chars().count();
        Self {
            groups: captures
                .iter()
                .map(|group| {
                    group.map(|group| {
                        (
                            group.as_str().to_string(),
                            chars(group.start()),
                            chars(group.end()),
                        )
                    })
                })
                .collect(),
        }
    }

    fn group(&self, index: i32) -> anyhow::Result<Option<&(String, usize, usize)>> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.groups.get(index))
            .map(Option::as_ref)
            .ok_or_else(|| anyhow::anyhow!("No group {index}"))
    }
}

impl fmt::Display for MatchValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.groups.first() {
            Some(Some((text, start, end))) => {
                write!(f, "<re.Match span=({start}, {end}) match={text:?}>")
            }
            _ => f.write_str("<re.Match>"),
        }
    }
}

// `starlark_value` requires a named lifetime.
#[expect(clippy::elidable_lifetime_names)]
#[starlark_value(type = "re.Match")]
impl<'v> StarlarkValue<'v> for MatchValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new();
        METHODS.methods(match_methods)
    }
}

fn found<'v>(heap: &'v Heap, text: &str, captures: Option<Captures>) -> Value<'v> {
    match captures {
        Some(captures) => heap.alloc(MatchValue::new(text, &captures)),
        None => Value::new_none(),
    }
}

#[starlark_module]
fn re_methods(builder: &mut MethodsBuilder) {
    /// The first match of `pattern` anywhere in `string`, or `None`.
    fn search<'v>(
        #[starlark(this)] _this: Value<'v>,
        pattern: &str,
        string: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        Ok(found(heap, string, compile(pattern)?.captures(string)))
    }

    /// A match of `pattern` at the start of `string`, or `None`.
    fn r#match<'v>(
        #[starlark(this)] _this: Value<'v>,
        pattern: &str,
        string: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        // The leftmost match starts at 0 if and only if there's a match at the start.
        let captures = compile(pattern)?
            .captures(string)
            .filter(|captures| captures.get(0).is_some_and(|whole| whole.start() == 0));
        Ok(found(heap, string, captures))
    }

    /// A match of `pattern` against all of `string`, or `None`.
    fn fullmatch<'v>(
        #[starlark(this)] _this: Value<'v>,
        pattern: &str,
        string: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let regex = compile(&fully_anchored(pattern)?)?;
        Ok(found(heap, string, regex.captures(string)))
    }

    /// The non-overlapping matches of `pattern` in `string`: the matched text if the pattern has
    /// no groups, the text of its group if it has one, and a tuple of its groups' texts
    /// otherwise.
    fn findall<'v>(
        #[starlark(this)] _this: Value<'v>,
        pattern: &str,
        string: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let regex = compile(pattern)?;
        let group = |captures: &Captures, index: usize| {
            heap.alloc(captures.get(index).map_or("", |group| group.as_str()))
        };
        let matches: Vec<Value<'v>> = regex
            .captures_iter(string)
            .map(|captures| match captures.len() {
                1 => group(&captures, 0),
                2 => group(&captures, 1),
                n => heap.alloc(AllocTuple((1..n).map(|index| group(&captures, index)))),
            })
            .collect();
        Ok(heap.alloc(AllocList(matches)))
    }

    /// Replaces the matches of `pattern` in `string` with `repl`, in which `$1` or `${name}`
    /// refer to groups. Replaces every match if `count` is 0.
    fn sub(
        #[starlark(this)] _this: Value<'_>,
        pattern: &str,
        repl: &str,
        string: &str,
        #[starlark(default = 0)] count: u32,
    ) -> anyhow::Result<String> {
        Ok(compile(pattern)?
            .replacen(string, count as usize, repl)
            .into_owned())
    }

    /// Splits `string` at the matches of `pattern`.
    fn split<'v>(
        #[starlark(this)] _this: Value<'v>,
        pattern: &str,
        string: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(AllocList(compile(pattern)?.split(string))))
    }

    /// Escapes the special characters of `string`, so that it matches itself.
    fn escape(#[starlark(this)] _this: Value<'_>, string: &str) -> anyhow::Result<String> {
        Ok(regex::escape(string))
    }
}

#[starlark_module]
fn match_methods(builder: &mut MethodsBuilder) {
    /// The text of group `index` (the whole match for 0), or `None` if the group didn't match.
    fn group<'v>(
        this: &MatchValue,
        #[starlark(default = 0)] index: i32,
    ) -> anyhow::Result<NoneOr<String>> {
        Ok(NoneOr::from_option(
            this.group(index)?.map(|(text, _, _)| text.clone()),
        ))
    }

    /// The texts of the groups after the whole match, with `None` for groups that didn't match.
    fn groups<'v>(this: &MatchValue, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        Ok(
            heap.alloc(AllocTuple(this.groups.iter().skip(1).map(|group| {
                group
                    .as_ref()
                    .map_or_else(Value::new_none, |(text, _, _)| heap.alloc(text.as_str()))
            }))),
        )
    }

    /// Where group `index` starts, or -1 if it didn't match.
    fn start(this: &MatchValue, #[starlark(default = 0)] index: i32) -> anyhow::Result<i64> {
        Ok(this.group(index)?.map_or(-1, |(_, start, _)| {
            i64::try_from(*start).unwrap_or(i64::MAX)
        }))
    }

    /// Where group `index` ends, or -1 if it didn't match.
    fn end(this: &MatchValue, #[starlark(default = 0)] index: i32) -> anyhow::Result<i64> {
        Ok(this
            .group(index)?
            .map_or(-1, |(_, _, end)| i64::try_from(*end).unwrap_or(i64::MAX)))
    }
}
