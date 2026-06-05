use hyper::Uri;

use regex::Regex;
use serde::{
    Deserializer,
    de::{
        self, DeserializeSeed, EnumAccess, Error as SerdeError, MapAccess, SeqAccess,
        VariantAccess, Visitor,
    },
    forward_to_deserialize_any,
};
use std::{
    any::type_name,
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    rc::Rc,
};
use tracing::trace;

use crate::{Error, Parsed, Part, VarMod, render::var_re_name};

// *** Time being, this is cribbed wholesale from Axum

// *** Pulled in from uses in Axum

// this wrapper type is used as the deserializer error to hide the `serde::de::Error` impl which
// would otherwise be public if we used `ErrorKind` as the error directly
#[derive(Debug, Clone)]
pub struct UriDeserializationError {
    pub(super) kind: ErrorKind,
}

impl UriDeserializationError {
    pub(super) fn new(kind: ErrorKind) -> Self {
        Self { kind }
    }

    pub(super) fn wrong_number_of_parameters(got: usize, expected: usize) -> Self {
        Self {
            kind: ErrorKind::WrongNumberOfParameters { got, expected },
        }
    }

    pub(super) fn cant_fill_parameters<T>(varlist: Vec<(Rc<str>, T)>, expected: usize) -> Self {
        let got = varlist
            .iter()
            .map(|(name, _)| name.as_ref().into())
            .collect();
        Self {
            kind: ErrorKind::CantFillParameters { got, expected },
        }
    }

    #[track_caller]
    pub(super) fn unsupported_type(name: &'static str) -> Self {
        Self::new(ErrorKind::UnsupportedType { name })
    }

    pub(super) fn mismatched_value(name: &'static str) -> Self {
        Self::new(ErrorKind::MismatchedValue {
            expected_type: name,
        })
    }
}

impl SerdeError for UriDeserializationError {
    #[inline]
    fn custom<T>(msg: T) -> Self
    where
        T: fmt::Display,
    {
        Self {
            kind: ErrorKind::Message(msg.to_string()),
        }
    }
}

impl fmt::Display for UriDeserializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for UriDeserializationError {}

#[derive(Debug, PartialEq, Eq, Clone)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The URI contained the wrong number of parameters.
    WrongNumberOfParameters {
        /// The number of actual parameters in the URI.
        got: usize,
        /// The number of expected parameters.
        expected: usize,
    },

    // The receiving variable can't hold the variables in the template
    CantFillParameters {
        got: Vec<String>,
        expected: usize,
    },

    /// Failed to parse the value at a specific key into the expected type.
    ///
    /// This variant is used when deserializing into types that have named fields, such as structs.
    ParseErrorAtKey {
        /// The key at which the value was located.
        key: String,
        /// The value from the URI.
        value: String,
        /// The expected type of the value.
        expected_type: &'static str,
    },

    /// Failed to parse the value at a specific index into the expected type.
    ///
    /// This variant is used when deserializing into sequence types, such as tuples.
    ParseErrorAtIndex {
        /// The index at which the value was located.
        index: usize,
        /// The value from the URI.
        value: String,
        /// The expected type of the value.
        expected_type: &'static str,
    },

    /// Failed to parse a value into the expected type.
    ///
    /// This variant is used when deserializing into a primitive type (such as `String` and `u32`).
    ParseError {
        /// The value from the URI.
        value: String,
        /// The expected type of the value.
        expected_type: &'static str,
    },

    /// Tried to serialize into an unsupported type such as nested maps.
    ///
    /// This error kind is caused by programmer errors and thus gets converted into a `500 Internal
    /// Server Error` response.
    UnsupportedType {
        /// The name of the unsupported type.
        name: &'static str,
    },

    MismatchedValue {
        expected_type: &'static str,
    },

    /// Catch-all variant for errors that don't fit any other variant.
    Message(String),
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Message(error) => error.fmt(f),
            ErrorKind::WrongNumberOfParameters { got, expected } => {
                write!(
                    f,
                    "Wrong number of path arguments for `Path`. Expected {expected} but got {got}"
                )?;

                if *expected == 1 {
                    write!(
                        f,
                        ". Note that multiple parameters must be extracted with a tuple `Path<(_, _)>` or a struct `Path<YourParams>`"
                    )?;
                }

                Ok(())
            }
            ErrorKind::CantFillParameters { got, expected } => {
                let count = got.len();
                write!(
                    f,
                    "Wrong number of path arguments for `Path`. Variables {got:?}({count}) but expected {expected}"
                )?;

                if *expected == 1 {
                    write!(
                        f,
                        ". Note that multiple parameters must be extracted with a tuple `Path<(_, _)>` or a struct `Path<YourParams>`"
                    )?;
                }

                Ok(())
            }
            ErrorKind::UnsupportedType { name } => write!(f, "Unsupported type `{name}`"),
            ErrorKind::MismatchedValue { expected_type } => {
                write!(f, "value not suitable for type `{expected_type}`")
            }
            ErrorKind::ParseErrorAtKey {
                key,
                value,
                expected_type,
            } => write!(
                f,
                "Cannot parse `{key}` with value `{value:?}` to a `{expected_type}`"
            ),
            ErrorKind::ParseError {
                value,
                expected_type,
            } => write!(f, "Cannot parse `{value:?}` to a `{expected_type}`"),
            ErrorKind::ParseErrorAtIndex {
                index,
                value,
                expected_type,
            } => write!(
                f,
                "Cannot parse value at index {index} with value `{value:?}` to a `{expected_type}`"
            ),
        }
    }
}

// *** end of inlining types

macro_rules! unsupported_type {
    ($trait_fn:ident) => {
        fn $trait_fn<V>(self, _: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            Err(UriDeserializationError::unsupported_type(type_name::<
                V::Value,
            >()))
        }
    };
}

macro_rules! parse_single_value {
    ($trait_fn:ident, $visit_fn:ident, $ty:literal) => {
        fn $trait_fn<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            if self.varlist.len() == 1 {
                let value = match &self.varlist[0] {
                    (_, VarBinding::Scalar(scalar)) => scalar.parse().map_err(|_| {
                        UriDeserializationError::new(ErrorKind::ParseError {
                            value: scalar.to_string(),
                            expected_type: $ty,
                        })
                    })?,
                    (_, VarBinding::PathExplode(_, string)) => string.parse().map_err(|_| {
                        UriDeserializationError::new(ErrorKind::ParseError {
                            value: string.to_string(),
                            expected_type: $ty,
                        })
                    })?,
                    (_, VarBinding::QueryExplode(_)) | (_, VarBinding::Assoc(_, _)) => {
                        return Err(UriDeserializationError::new(ErrorKind::MismatchedValue {
                            expected_type: $ty,
                        }));
                    }
                    (_, VarBinding::Empty) => {
                        return Err(UriDeserializationError::new(ErrorKind::ParseError {
                            value: "<empty>".to_string(),
                            expected_type: $ty,
                        }));
                    }
                };
                visitor.$visit_fn(value)
            } else {
                return Err(UriDeserializationError::cant_fill_parameters(
                    self.varlist,
                    1,
                ));
            }
        }
    };
}

/// Convert a byte string in the `application/x-www-form-urlencoded` syntax
/// into a iterator of (name, value) pairs.
///
/// Use `parse(input.as_bytes())` to parse a `&str` string.
///
/// The names and values are percent-decoded. For instance, `%23first=%25try%25` will be
/// converted to `[("#first", "%try%")]`.
///
/// Gratefully lifted from form_urlencoded, because we needed to add path parameter parsing as well
#[inline]
pub fn parse(sep: u8, input: &[u8]) -> Parse<'_> {
    Parse { sep, input }
}
/// The return type of `parse()`.
#[derive(Copy, Clone)]
pub struct Parse<'a> {
    sep: u8,
    input: &'a [u8],
}

impl<'a> Iterator for Parse<'a> {
    type Item = (Cow<'a, str>, Cow<'a, str>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.input.is_empty() {
                return None;
            }
            let mut split2 = self.input.splitn(2, |&b| b == self.sep);
            let sequence = split2.next().unwrap();
            self.input = split2.next().unwrap_or(&[][..]);
            if sequence.is_empty() {
                continue;
            }
            let mut split2 = sequence.splitn(2, |&b| b == b'=');
            let name = split2.next().unwrap();
            let value = split2.next().unwrap_or(&[][..]);
            return Some((decode(name), decode(value)));
        }
    }
}

fn decode(input: &[u8]) -> Cow<'_, str> {
    let replaced = replace_plus(input);
    decode_utf8_lossy(match percent_encoding::percent_decode(&replaced).into() {
        Cow::Owned(vec) => Cow::Owned(vec),
        Cow::Borrowed(_) => replaced,
    })
}

fn decode_utf8_lossy(input: Cow<'_, [u8]>) -> Cow<'_, str> {
    // Note: This function is duplicated in `form_urlencoded/src/query_encoding.rs`.
    match input {
        Cow::Borrowed(bytes) => String::from_utf8_lossy(bytes),
        Cow::Owned(bytes) => {
            match String::from_utf8_lossy(&bytes) {
                Cow::Borrowed(utf8) => {
                    // If from_utf8_lossy returns a Cow::Borrowed, then we can
                    // be sure our original bytes were valid UTF-8. This is because
                    // if the bytes were invalid UTF-8 from_utf8_lossy would have
                    // to allocate a new owned string to back the Cow so it could
                    // replace invalid bytes with a placeholder.

                    // First we do a debug_assert to confirm our description above.
                    let raw_utf8: *const [u8] = utf8.as_bytes();
                    debug_assert!(core::ptr::eq(raw_utf8, &*bytes));

                    // Given we know the original input bytes are valid UTF-8,
                    // and we have ownership of those bytes, we re-use them and
                    // return a Cow::Owned here.
                    Cow::Owned(unsafe { String::from_utf8_unchecked(bytes) })
                }
                Cow::Owned(s) => Cow::Owned(s),
            }
        }
    }
}
/// Replace b'+' with b' '
fn replace_plus(input: &[u8]) -> Cow<'_, [u8]> {
    match input.iter().position(|&b| b == b'+') {
        None => Cow::Borrowed(input),
        Some(first_position) => {
            let mut replaced = input.to_owned();
            replaced[first_position] = b' ';
            for byte in &mut replaced[first_position + 1..] {
                if *byte == b'+' {
                    *byte = b' ';
                }
            }
            Cow::Owned(replaced)
        }
    }
}

#[derive(Clone)]
pub(crate) struct UriDeserializer {
    varlist: Vec<(Rc<str>, VarBinding)>,
}

/*
impl UriDeserializer {
    #[inline]
    fn new(varlist: Vec<(Rc<str>, VarBinding)>) -> Self {
        UriDeserializer { varlist }
    }
}
*/

#[derive(Clone, Debug)]
enum VarBinding {
    Scalar(Rc<str>),
    PathExplode(Rc<str>, Rc<str>), // sep, string value
    QueryExplode(Vec<Rc<str>>),
    Assoc(Rc<str>, Vec<(Rc<str>, Rc<str>)>), // join (&,;), pairs of k/v
    Empty,
}

impl VarBinding {
    fn to_str(&self) -> Result<Rc<str>, UriDeserializationError> {
        match self {
            VarBinding::Scalar(v) => Ok(v.clone()),
            VarBinding::PathExplode(_, joined) => Ok(joined.clone()),
            VarBinding::QueryExplode(items) => Ok(items.join(",").into()),
            VarBinding::Assoc(sep, list) => Ok(list
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(sep)
                .into()),
            VarBinding::Empty => Ok("".into()),
        }
    }
}

fn parsed_template_vars(parsed: &Parsed) -> Vec<Rc<str>> {
    parsed
        .parts_iter()
        .filter_map(|part| match part {
            Part::Lit(_) => None,
            Part::Expression(expression)
            | Part::SegPathVar(expression)
            | Part::SegRest(expression)
            | Part::SegPathRest(expression) => Some(
                expression
                    .varspecs
                    .iter()
                    .map(|vs| Rc::<str>::from(vs.varname.clone())) // XXX consider Rc<str> in VarSpec
                    .collect::<Vec<_>>(),
            ),
        })
        .flatten()
        .collect::<Vec<_>>()
}

//   * Parser Path Scalar names
//   Need to emit (capture,varname) - usually (x,x) but sometimes (x,x_p3)
fn path_scalar_names(parsed: &Parsed) -> HashMap<Rc<str>, Rc<str>> {
    let mut result = HashMap::new();
    for part in parsed.nonquery_parts_iter() {
        match part.expression() {
            // PATH
            Some(exp) => {
                for v in &exp.varspecs {
                    match v.modifier {
                        VarMod::None => {
                            let name: Rc<str> = v.varname.clone().into();
                            result.insert(name.clone(), name); // SCALAR
                        }
                        VarMod::Prefix(_) => {
                            // XXX edge case: should be the longest prefix
                            result.insert(v.varname.clone().into(), var_re_name(&v).into());
                        }
                        VarMod::Explode => (),
                    }
                }
            }
            None => (),
        }
    }
    result
}
//   * Parser Query Scalar names
fn query_scalar_names(parsed: &Parsed) -> HashSet<&str> {
    parsed
        .query_parts_iter()
        .filter_map(|part| {
            part.expression().map(|exp| {
                exp.varspecs
                    .iter()
                    .filter_map(|v| match v.modifier {
                        VarMod::None | VarMod::Prefix(_) => Some(v.varname.as_str()), // SCALAR
                        _ => None,
                    })
                    .collect::<Vec<&str>>()
            })
        })
        .flatten()
        .collect()
}
//   * Parser Path Explode names - might need the joiner in order to split them
fn path_explode_names(parsed: &Parsed) -> Vec<(&str, &str)> {
    parsed
        .nonquery_parts_iter()
        .filter_map(|part| {
            part.expression().map(|exp| {
                exp.varspecs
                    .iter()
                    .filter_map(|v| match v.modifier {
                        VarMod::Explode => Some((exp.operator.separator(), v.varname.as_str())), // EXPLODE
                        _ => None,
                    })
                    .collect::<Vec<(&str, &str)>>()
            })
        })
        .flatten()
        .collect()
}
//   * Parser Query Explode names
fn query_explode_names(parsed: &Parsed) -> HashSet<&str> {
    parsed
        .query_parts_iter()
        .filter_map(|part| {
            part.expression().map(|exp| {
                exp.varspecs
                    .iter()
                    .filter_map(|v| match v.modifier {
                        VarMod::Explode => Some(v.varname.as_str()), // EXPLODE
                        _ => None,
                    })
                    .collect::<Vec<&str>>()
            })
        })
        .flatten()
        .collect()
}

impl UriDeserializer {
    // XXX &Uri?
    //  Principle: RFC 6570 is the upper limit on features we support. We have to constrain
    //  from there in order to be able to deserialize, but there's no "trade" for more flexibility
    //  above what we allow
    //
    //  One principle here: I should be able to round trip a serde struct through "its" URITemplate;
    //
    // The deserializer will need:
    // * KV of scalars
    // * KV of (joiner, list) or (scalar, split list) from path explodes
    // * KV of lists from query explodes
    // Path lists need their joiner provided
    // Note: queries can provide keys not in the template,
    // because {?assoc*} (only that case) ({;assoc*}?)
    //
    // During URI parsing,
    // * same key with different values: Error
    // * path lists in preference to query lists
    //
    // Deserialization: exactly one map/struct can consume
    // {?assoc*} fields. Rule is:
    //
    // Field is named in template: it gets the value provided
    // One field (map/struct) not named in template: all the unnamed scalars
    // A second complex field not named in template: Error
    //
    // Explicitly ignoring order of visit, e.g. with Default rules
    // (derive(Route) should be able to compare template to fields and (possibly) panic)
    // Note: template doesn't know field types,
    // so multiple explode _could_ all (or all but one) be lists.
    // The URI presented also doesn't make it clear: multiple missing explodes... might just be empty lists
    //
    // In that case, the URI and struct are in tension, which in the arbitrary case
    // is an unfortunate runtime problem. Again, the round-trip principle comes to play here.
    //
    // So, best efforts on matching runtime data from other sources to structs like this:
    // path list with scalar field: gets the value as provided (e.g. "/trailing/path")
    //  with seq field, split with joiner (e.g. ["trailing", "path"])
    //
    // query list with scalar field: error from Deserializer
    //
    // scalar with seq field: split on ,
    // scalar with map field: split on , into pairs on =, or error from Deserializer
    //
    // Part of the logic here:
    //
    //                     DeserializeTarget
    // Template                scalar       seq             map
    // --------------------+------------+----------------+--------
    // Mod::None           |   idem     |   split(",")   | Error
    // Path, Mod::Explode  | "/seg/seg" | split(joiner)  | split pairs on =
    // Query, Mod::Explode |  Error     | matching vals  | "loose" keys
    //
    //
    //
    // How should we handle prefix captures in regex (e.g. foo_p5)?
    // How should we handle prefixes in queries?
    //
    // IMO {?var:3} is a _mistake_ not an error
    // 6570 doesn't have a way to rename that field, so e.g.
    // {?var:3,var} would -> ?var=val&var=value
    //
    // We treat an isolated prefixed template value as the value;
    // the author needs to work with that (or use a different template).
    // Reuse works the same as any other variable - a prefix variable
    // needs to be a prefix of a longer prefix or unprefixed variable
    // or the URI doesn't match the template. (maybe could be made part of the Regex?)
    //
    // Query prefix should be checked by derive(Route) and here.
    //
    //  use _values_ to check prefixing and length
    //  IOW: if a variable repeats, new value must be a prefix of old
    //  or v/v. Longest is new value. Not a prefix? Error.
    //  For same length, devolves to equality, and we don't have to record
    //  anything about what the template said.
    //
    //  XXX It's an error to reuse a capture group name in Regex, so we should use capture matches
    //  (if that's what they're called?)
    //
    //  XXX As is, a varname can be both a scalar and complex, which seems like a weird edge case
    //      That's true both within path/query, and between (i.e. scalar path, list query)
    //      However!! that's a problem with template, which should be validated separately
    //      (and before it goes into the main MAP
    //
    //  Key realization: template issues should already have been validated
    //  * TODO: template validation post-parse
    //  * Template errors here can panic(unreachable!()) or return 500s
    //  * URI errors (e.g. same variable, different values) should raise errors
    //
    //  FUTURE: handle Path parameters ({;list*} or {;assoc*})
    //
    //  Should I do my own parsing of parameters? Possibly adopt the Mozilla module?
    //  We could also parse to Rc<str> if we keep going that, or &str.
    //
    //  We can only allow one assoc-explode query param, (or path param in a "run".) In those cases
    //  we'll see keys that don't match expressions in the template, and then gather them into a
    //  map for the expression.
    //
    /// Creates a UriDeserializer for a Uri, a Parsed route template and the compiled regex for
    /// the Parsed
    /// .
    ///
    /// # Panics
    ///
    /// Panics if the template has invalid overlaps between scalar and complex variables.
    ///
    /// # Errors
    ///
    /// This function will return an error if a valid context for deserialization
    /// can't be extracted from the URI
    pub(crate) fn for_uri(
        uri: &Uri,
        parsed: &Parsed,
        regex: &Regex,
    ) -> Result<UriDeserializer, Error> {
        trace!("Setting up to deserialize {uri:?} via {parsed:?}");
        // Scalar values, regardless of source
        let mut scalars: HashMap<Rc<str>, Rc<str>> = HashMap::new();
        // Explodes from the path
        let mut path_explodes: HashMap<Rc<str>, (Rc<str>, Rc<str>)> = HashMap::new();
        // Explodes from the query
        let mut query_explodes: HashMap<Rc<str>, Vec<Rc<str>>> = HashMap::new();
        // Assoc explodes
        let mut assocs: HashMap<Rc<str>, Vec<(Rc<str>, Rc<str>)>> = HashMap::new();

        // Extract values from the URI path (and possibly scheme/auth)
        let path = uri.path();
        let url = match (uri.scheme_str(), uri.authority().map(|a| a.as_str())) {
            (Some(scheme), Some(auth)) => &format!("{scheme}://{auth}{path}"),
            (None, Some(auth)) => &format!("//{auth}{path}"),
            (Some(scheme), None) => &format!("{scheme}://{path}"),
            (None, None) => path,
        };

        // Local lambda to capture scalars
        // Key concern is that prefixed values have to match up properly
        // i/o/w /{foo:3}/{foo} -> /exa/example, not /zzz/yyyyyyy
        let mut new_scalar = |var_name: Rc<str>, m: Rc<str>| -> Result<(), Error> {
            if let Some(new_val) = percent_decode(m) {
                if let Some(val) = scalars.get(&var_name) {
                    if val.len() > new_val.len() {
                        if !val.starts_with(&*new_val) {
                            return Err(Error::MismatchedValues(
                                var_name.to_string(),
                                val.to_string(),
                                new_val.to_string(),
                            ));
                        } // else we already have the longest value
                    } else if new_val.starts_with(val.as_ref()) {
                        scalars.insert(var_name, new_val);
                    } else {
                        return Err(Error::MismatchedValues(
                            var_name.to_string(),
                            val.to_string(),
                            new_val.to_string(),
                        ));
                    }
                } else {
                    scalars.insert(var_name, new_val);
                }
            };
            Ok(())
        };

        // definitely considering a secondary Nom parser instead of RE here.
        let caps = regex
            .captures(url)
            .ok_or_else(|| Error::NoMatch(url.to_string()))?;

        // Extract values from the URI query
        let query_parse = match (parsed.query.is_some(), uri.query()) {
            (false, None) |
            // XXX consider: optional queries - should we make you get(var) from a HashMap?
            (false, Some(_)) | // XXX Error? (if you want to accept abitrary queries, {?ignored*}
            (true,  None) => parse(b'&', &[]),
            (true, Some(q)) => parse(b'&', q.as_bytes()),
        };

        let query_scalar_names = query_scalar_names(parsed);
        let query_explode_names = query_explode_names(parsed);
        let mut query_assocs: Vec<(Rc<str>, Rc<str>)> = vec![];
        for (cow_var_name, cow_value) in query_parse {
            let var_name: Rc<str> = cow_var_name.into_owned().into();
            let value: Rc<str> = cow_value.into_owned().into();
            (var_name.clone(), value.clone());

            if query_scalar_names.contains(var_name.as_ref()) {
                new_scalar(var_name, value)?
            } else if query_explode_names.contains(var_name.as_ref()) {
                query_explodes
                    .entry(var_name)
                    .and_modify(|exes| exes.push(value.clone()))
                    .or_insert_with(|| vec![value.clone()]);
            } else {
                query_assocs.push((var_name, value));
            }
        }
        // We can't know until we're deserializing what the target field is;
        // it's tempting to look for a "last" explode name and put the assocs
        // into a "VarBinding::Assoc" or the like, but empty lists at "fill"
        // time would be simply omitted, so we can't make that assumption.
        // Instead, we have to look for a single map in the target.
        for x_name in query_explode_names {
            query_explodes.entry(x_name.into()).or_default();
        }

        if let Some(name) = parsed.query_assoc_name() {
            assocs.insert(name, query_assocs.clone());
        } else if (&query_assocs).len() > 0 {
            return Err(Error::UnexpectedVariables(
                query_assocs.iter().map(|(k, _)| k.to_string()).collect(),
            ));
        }

        let mut path_assocs: Vec<(Rc<str>, Rc<str>)> = vec![];
        let names = path_scalar_names(parsed);
        for (var_name, re_name) in names {
            if let Some(m) = caps.name(&re_name) {
                new_scalar(var_name, m.as_str().into())?
            }
        }
        for (sep, var_name) in path_explode_names(parsed) {
            if let Some(m) = caps.name(&var_name) {
                if Some(var_name) == parsed.path_assoc_name().as_deref() {
                    let path_parse = parse(
                        // XXX ick
                        sep.bytes().nth(0).expect("seps to be single byte strings"),
                        m.as_str().as_bytes(),
                    );
                    path_assocs = path_parse
                        .map(|(k, v)| (k.into_owned().into(), v.into_owned().into()))
                        .collect();
                } else {
                    if let Some(dec) = percent_decode(m.as_str()) {
                        path_explodes.insert(var_name.into(), (sep.into(), dec));
                    }
                }
            }
        }

        if let Some(name) = parsed.path_assoc_name() {
            assocs.insert(name, path_assocs.clone());
        } else if (&path_assocs).len() > 0 {
            return Err(Error::UnexpectedVariables(
                path_assocs.iter().map(|(k, _)| k.to_string()).collect(),
            ));
        }

        let varlist = parsed_template_vars(parsed)
            .iter()
            .map(move |vname| {
                match (
                    scalars.get(vname),
                    path_explodes.get(vname),
                    query_explodes.get(vname),
                    assocs.get(vname),
                ) {
                    (Some(scalar), None, None, None) => {
                        Ok((vname.clone(), VarBinding::Scalar(scalar.clone())))
                    }
                    (None, Some((sep, string)), None, None) => Ok((
                        vname.clone(),
                        VarBinding::PathExplode(sep.clone(), string.clone()),
                    )),
                    (None, None, Some(query), None) => {
                        if query.len() > 0 {
                            Ok((vname.clone(), VarBinding::QueryExplode(query.clone())))
                        } else {
                            Ok((vname.clone(), VarBinding::Empty))
                        }
                    }
                    (None, Some((sep, string)), Some(query), None) => {
                        let path_vec = string
                            .split(sep.as_ref())
                            .map(|part| part.into())
                            .collect::<Vec<Rc<str>>>();
                        if query.len() > 0 {
                            if path_vec == *query {
                                Ok((
                                    vname.clone(),
                                    VarBinding::PathExplode(sep.clone(), string.clone()),
                                ))
                            } else {
                                Err(Error::MismatchedValues(
                                    vname.to_string(),
                                    string.to_string(),
                                    query.join(sep),
                                ))
                            }
                        } else {
                            Err(Error::MismatchedValues(
                                vname.to_string(),
                                string.to_string(),
                                "".to_string(),
                            ))
                        }
                    }
                    (None, None, None, Some(assoc)) => {
                        Ok((vname.clone(), VarBinding::Assoc("?".into(), assoc.clone())))
                    }
                    (None, None, None, None) => Ok((vname.clone(), VarBinding::Empty)),
                    (None, None, Some(q), Some(assoc)) => {
                        if q.len() > 0 {
                            Err(Error::UnexpectedVariables(vec![vname.to_string()]))
                        } else {
                            Ok((vname.clone(), VarBinding::Assoc("?".into(), assoc.clone())))
                        }
                    }
                    (Some(_), Some(_), _, _) => {
                        panic!("scalar and path explode should be caught in template parse")
                    }
                    (Some(_), _, Some(_), _) => {
                        panic!("scalar and query explode should be caught in template parse")
                    }
                    (None, Some(_), _, Some(_)) => {
                        panic!("path explode and query assoc should be caught in parse")
                    }
                    (Some(_), None, None, Some(_)) => {
                        panic!("query assoc and scalar overlap should be caught in parse")
                    }
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(UriDeserializer { varlist })
    }
}

fn percent_decode<S: AsRef<str>>(s: S) -> Option<Rc<str>> {
    percent_encoding::percent_decode(s.as_ref().as_bytes())
        .decode_utf8()
        .ok() //consider: Result?
        .map(|decoded| decoded.as_ref().into())
}

impl<'de> Deserializer<'de> for UriDeserializer {
    type Error = UriDeserializationError;

    unsupported_type!(deserialize_bytes);
    unsupported_type!(deserialize_option);
    unsupported_type!(deserialize_identifier);
    unsupported_type!(deserialize_ignored_any);

    parse_single_value!(deserialize_bool, visit_bool, "bool");
    parse_single_value!(deserialize_i8, visit_i8, "i8");
    parse_single_value!(deserialize_i16, visit_i16, "i16");
    parse_single_value!(deserialize_i32, visit_i32, "i32");
    parse_single_value!(deserialize_i64, visit_i64, "i64");
    parse_single_value!(deserialize_i128, visit_i128, "i128");
    parse_single_value!(deserialize_u8, visit_u8, "u8");
    parse_single_value!(deserialize_u16, visit_u16, "u16");
    parse_single_value!(deserialize_u32, visit_u32, "u32");
    parse_single_value!(deserialize_u64, visit_u64, "u64");
    parse_single_value!(deserialize_u128, visit_u128, "u128");
    parse_single_value!(deserialize_f32, visit_f32, "f32");
    parse_single_value!(deserialize_f64, visit_f64, "f64");
    parse_single_value!(deserialize_string, visit_string, "String");
    parse_single_value!(deserialize_byte_buf, visit_string, "String");
    parse_single_value!(deserialize_char, visit_char, "char");

    fn deserialize_any<V>(self, v: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_str(v)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if let [(_, binding)] = self.varlist.as_slice() {
            visitor.visit_str(&binding.to_str()?)
        } else {
            return Err(UriDeserializationError::cant_fill_parameters(
                self.varlist,
                1,
            ));
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.varlist.as_slice() {
            [(name, VarBinding::PathExplode(sep, s))] => visitor.visit_seq(SeqDeserializer {
                params: s
                    .split(sep.as_ref())
                    .map(|item| (name.clone(), VarBinding::Scalar(item.into())))
                    .collect(),
                idx: 0,
            }),
            [(name, VarBinding::QueryExplode(l))] => visitor.visit_seq(SeqDeserializer {
                params: l
                    .iter()
                    .map(|item| (name.clone(), VarBinding::Scalar(item.clone())))
                    .collect(),
                idx: 0,
            }),
            _ => visitor.visit_seq(SeqDeserializer {
                params: self.varlist.into(),
                idx: 0,
            }),
        }
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.varlist.as_slice() {
            [(name, VarBinding::PathExplode(sep, s))] => visitor.visit_seq(SeqDeserializer {
                params: s
                    .split(sep.as_ref())
                    .map(|item| (name.clone(), VarBinding::Scalar(item.into())))
                    .collect(),
                idx: 0,
            }),
            [(name, VarBinding::QueryExplode(l))] => {
                if l.len() < len {
                    return Err(UriDeserializationError::wrong_number_of_parameters(
                        l.len(),
                        len,
                    ));
                }
                visitor.visit_seq(SeqDeserializer {
                    params: l
                        .iter()
                        .map(|item| (name.clone(), VarBinding::Scalar(item.clone())))
                        .collect(),
                    idx: 0,
                })
            }
            _ => {
                if self.varlist.len() < len {
                    return Err(UriDeserializationError::cant_fill_parameters(
                        self.varlist,
                        len,
                    ));
                }
                visitor.visit_seq(SeqDeserializer {
                    params: self.varlist.into(),
                    idx: 0,
                })
            }
        }
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(MapDeserializer {
            params: self.varlist.into(),
            value: None,
            key: None,
        })
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.varlist.as_slice() {
            [(_, VarBinding::Scalar(value))] => visitor.visit_enum(EnumDeserializer {
                value: value.clone(),
            }),
            [(_, val)] => {
                return Err(UriDeserializationError::invalid_value(
                    de::Unexpected::Str(&val.to_str()?),
                    &"an enum variant",
                ));
            }
            _ => {
                return Err(UriDeserializationError::cant_fill_parameters(
                    self.varlist,
                    1,
                ));
            }
        }
    }
}

struct MapDeserializer {
    params: VecDeque<(Rc<str>, VarBinding)>,
    key: Option<KeyOrIdx>,
    value: Option<VarBinding>,
}

// XXX consider implementing next_entry_seed, which would let us skip the option/take dance
impl<'de> MapAccess<'de> for MapDeserializer {
    type Error = UriDeserializationError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        match self.params.pop_front() {
            Some((key, value)) => {
                self.key = Some(KeyOrIdx::Key(key.clone()));
                self.value = Some(value);
                seed.deserialize(KeyDeserializer { key }).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        match self.value.take() {
            Some(value) => seed.deserialize(ValueDeserializer {
                key: self.key.take(),
                value,
            }),
            None => Err(UriDeserializationError::custom("value is missing")),
        }
    }

    fn next_entry_seed<K, V>(
        &mut self,
        kseed: K,
        vseed: V,
    ) -> Result<Option<(K::Value, V::Value)>, Self::Error>
    where
        K: DeserializeSeed<'de>,
        V: DeserializeSeed<'de>,
    {
        match self.params.pop_front() {
            Some((key, value)) => {
                let vk = KeyOrIdx::Key(key.clone());
                Ok(Some((
                    kseed.deserialize(KeyDeserializer { key })?,
                    vseed.deserialize(ValueDeserializer {
                        key: Some(vk),
                        value,
                    })?,
                )))
            }
            None => Ok(None),
        }
    }
}

struct KeyDeserializer {
    key: Rc<str>,
}

macro_rules! parse_key {
    ($trait_fn:ident) => {
        fn $trait_fn<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            visitor.visit_str(&self.key)
        }
    };
}

impl<'de> Deserializer<'de> for KeyDeserializer {
    type Error = UriDeserializationError;

    parse_key!(deserialize_identifier);
    parse_key!(deserialize_str);
    parse_key!(deserialize_string);

    fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(UriDeserializationError::custom("Unexpected key type"))
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char bytes
        byte_buf option unit unit_struct seq tuple
        tuple_struct map newtype_struct struct enum ignored_any
    }
}

macro_rules! parse_value {
    ($trait_fn:ident, $visit_fn:ident, $ty:literal) => {
        fn $trait_fn<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            let valstr = self.value.to_str()?;
            let v = valstr.parse().map_err(|_| {
                if let Some(key) = self.key.take() {
                    let kind = match key {
                        KeyOrIdx::Key(key) => ErrorKind::ParseErrorAtKey {
                            key: key.to_string(),
                            value: valstr.to_string(),
                            expected_type: $ty,
                        },
                        KeyOrIdx::Idx { idx: index, key: _ } => ErrorKind::ParseErrorAtIndex {
                            index,
                            value: valstr.to_string(),
                            expected_type: $ty,
                        },
                    };
                    UriDeserializationError::new(kind)
                } else {
                    UriDeserializationError::new(ErrorKind::ParseError {
                        value: valstr.to_string(),
                        expected_type: $ty,
                    })
                }
            })?;
            visitor.$visit_fn(v)
        }
    };
}

#[derive(Debug)]
struct ValueDeserializer {
    key: Option<KeyOrIdx>,
    value: VarBinding,
}

impl<'de> Deserializer<'de> for ValueDeserializer {
    type Error = UriDeserializationError;

    unsupported_type!(deserialize_identifier);

    parse_value!(deserialize_bool, visit_bool, "bool");
    parse_value!(deserialize_i8, visit_i8, "i8");
    parse_value!(deserialize_i16, visit_i16, "i16");
    parse_value!(deserialize_i32, visit_i32, "i32");
    parse_value!(deserialize_i64, visit_i64, "i64");
    parse_value!(deserialize_i128, visit_i128, "i128");
    parse_value!(deserialize_u8, visit_u8, "u8");
    parse_value!(deserialize_u16, visit_u16, "u16");
    parse_value!(deserialize_u32, visit_u32, "u32");
    parse_value!(deserialize_u64, visit_u64, "u64");
    parse_value!(deserialize_u128, visit_u128, "u128");
    parse_value!(deserialize_f32, visit_f32, "f32");
    parse_value!(deserialize_f64, visit_f64, "f64");
    parse_value!(deserialize_string, visit_string, "String");
    parse_value!(deserialize_byte_buf, visit_string, "String");
    parse_value!(deserialize_char, visit_char, "char");

    fn deserialize_any<V>(self, v: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_str(v)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str(&self.value.to_str()?)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_bytes(self.value.to_str()?.as_bytes())
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        struct PairDeserializer {
            key: Option<KeyOrIdx>,
            value: Option<VarBinding>,
        }

        impl<'de> SeqAccess<'de> for PairDeserializer {
            type Error = UriDeserializationError;

            fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
            where
                T: DeserializeSeed<'de>,
            {
                match self.key.take() {
                    Some(KeyOrIdx::Idx { idx: _, key }) => {
                        return seed.deserialize(KeyDeserializer { key }).map(Some);
                    }
                    // `KeyOrIdx::Key` is only used when deserializing maps so `deserialize_seq`
                    // wouldn't be called for that
                    Some(KeyOrIdx::Key(_)) => unreachable!(),
                    None => {}
                };

                self.value
                    .take()
                    .map(|value| seed.deserialize(ValueDeserializer { key: None, value }))
                    .transpose()
            }
        }

        if len == 2 {
            match self.key {
                Some(key) => visitor.visit_seq(PairDeserializer {
                    key: Some(key),
                    value: Some(self.value),
                }),
                // `self.key` is only `None` when deserializing maps so `deserialize_seq`
                // wouldn't be called for that
                None => unreachable!(),
            }
        } else {
            Err(UriDeserializationError::unsupported_type(type_name::<
                V::Value,
            >()))
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            VarBinding::Assoc(_, items) => visitor.visit_map(MapDeserializer {
                params: items
                    .iter()
                    .map(|(k, value)| (k.clone(), VarBinding::Scalar(value.clone())))
                    .collect(),
                value: None,
                key: None,
            }),
            VarBinding::Empty => visitor.visit_unit(),

            VarBinding::Scalar(_) | VarBinding::PathExplode(_, _) | VarBinding::QueryExplode(_) => {
                Err(UriDeserializationError::mismatched_value("non-map value"))
            }
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            VarBinding::Scalar(_) => Err(UriDeserializationError::unsupported_type(type_name::<
                V::Value,
            >(
            ))),
            VarBinding::PathExplode(sep, item_string) => visitor.visit_seq(SeqDeserializer {
                params: item_string
                    .split(sep.as_ref())
                    .enumerate()
                    .map(|(idx, v)| (format!("{idx}").into(), VarBinding::Scalar(v.into())))
                    .collect(),
                idx: 0,
            }),
            VarBinding::QueryExplode(items) => visitor.visit_seq(SeqDeserializer {
                params: items
                    .iter()
                    .enumerate()
                    .map(|(idx, v)| (format!("{idx}").into(), VarBinding::Scalar(v.clone())))
                    .collect(),
                idx: 0,
            }),
            VarBinding::Assoc(_, items) => visitor.visit_seq(SeqDeserializer {
                params: items
                    .iter()
                    .map(|(key, value)| (key.clone(), VarBinding::Scalar(value.clone())))
                    .collect(),
                idx: 0,
            }),
            VarBinding::Empty => visitor.visit_unit(),
        }
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(UriDeserializationError::unsupported_type(type_name::<
            V::Value,
        >()))
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_enum(EnumDeserializer {
            value: self.value.to_str()?,
        })
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }
}

struct EnumDeserializer {
    value: Rc<str>,
}

impl<'de> EnumAccess<'de> for EnumDeserializer {
    type Error = UriDeserializationError;
    type Variant = UnitVariant;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        Ok((
            seed.deserialize(KeyDeserializer { key: self.value })?,
            UnitVariant,
        ))
    }
}

struct UnitVariant;

impl<'de> VariantAccess<'de> for UnitVariant {
    type Error = UriDeserializationError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        Err(UriDeserializationError::unsupported_type(
            "newtype enum variant",
        ))
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(UriDeserializationError::unsupported_type(
            "tuple enum variant",
        ))
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(UriDeserializationError::unsupported_type(
            "struct enum variant",
        ))
    }
}

struct SeqDeserializer {
    params: VecDeque<(Rc<str>, VarBinding)>,
    idx: usize,
}

impl<'de> SeqAccess<'de> for SeqDeserializer {
    type Error = UriDeserializationError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        match self.params.pop_front() {
            Some((key, value)) => {
                let idx = self.idx;
                self.idx += 1;
                Ok(Some(seed.deserialize(ValueDeserializer {
                    key: Some(KeyOrIdx::Idx { idx, key }),
                    value: value,
                })?))
            }
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone)]
enum KeyOrIdx {
    Key(Rc<str>),
    Idx { idx: usize, key: Rc<str> },
}

#[cfg(test)]
mod tests {
    use crate::ResourceMappingString;
    use crate::Serde6570;

    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    enum MyEnum {
        A,
        B,
        #[serde(rename = "c")]
        C,
    }

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    struct Struct {
        c: String,
        b: bool,
        a: i32,
    }

    fn create_url_params<I, K, V>(values: I) -> Vec<(Rc<str>, VarBinding)>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        values
            .into_iter()
            .map(|(k, v)| {
                (
                    Rc::from(k.as_ref()),
                    VarBinding::Scalar(Rc::from(v.as_ref())),
                )
            })
            .collect()
    }

    macro_rules! check_single_value {
        ($ty:ty, $value_str:literal, $value:expr) => {
            #[allow(clippy::bool_assert_comparison)]
            {
                let url_params = create_url_params(vec![("value", $value_str)]);
                let deserializer = UriDeserializer {
                    varlist: url_params,
                };
                assert_eq!(<$ty>::deserialize(deserializer).unwrap(), $value);
            }
        };
    }

    // XXX A template with an explode modified variable
    //    * either is "filled" is a list
    //    * or filled with a map, and the variable is tagged as assoc.
    #[test]
    fn test_real_search_template() {
        //let rt = RouteTemplateString("/search{?query,kind}".into(), vec![]);
        let rt = ResourceMappingString("/search{?query,extra*}".into(), vec!["extra".into()]);

        let cfg = crate::process(rt).expect("test case to parse");
        let uri = hyper::Uri::from_static("http://example.com/search?query=test");
        let search: HashMap<String, String> = cfg.contract(uri.clone()).expect("deserialize");
        assert_eq!(
            "test",
            search.get("query").expect("query to be deserialized")
        );
        let uri = hyper::Uri::from_static("http://example.com/search?query=test&kind=boardgame");
        let search: HashMap<String, String> = cfg.contract(uri.clone()).expect("deserialize");
        assert_eq!(
            "test",
            search.get("query").expect("query to be deserialized")
        );
        assert_eq!(
            "kind=boardgame",
            search.get("extra").expect("kind to be deserialized")
        )
    }

    #[test]
    fn test_path_style_parameter() {
        let rt = ResourceMappingString("/search/{;query,extra*}".into(), vec!["extra".into()]);
        let cfg = crate::process(rt).expect("test case to parse");
        let uri = hyper::Uri::from_static("http://example.com/search/;query=test;kind=boardgame");
        let search: HashMap<String, String> = cfg.contract(uri.clone()).expect("deserialize");
        assert_eq!(
            "test",
            search.get("query").expect("query to be deserialized")
        );
        assert_eq!(
            "kind=boardgame",
            search.get("extra").expect("kind to be deserialized")
        );
        #[derive(Deserialize, Debug)]
        struct Search {
            query: String,
            extra: HashMap<String, String>,
        }
        let search_struct: Search = cfg.contract(uri.clone()).expect("deserialize");
        assert_eq!("test", &search_struct.query);
        assert_eq!(
            "boardgame",
            search_struct
                .extra
                .get("kind")
                .expect("kind to be deserialized")
        )
    }

    #[test]
    fn test_path_segment_assoc() {
        let rt = ResourceMappingString("/search{/extra*}".into(), vec!["extra".into()]);
        let cfg = crate::process(rt).expect("test case to parse");
        let uri = hyper::Uri::from_static("http://example.com/search/query=test/kind=boardgame");
        #[derive(Deserialize, Debug)]
        struct Search {
            extra: HashMap<String, String>,
        }
        let search_struct: Search = cfg.contract(uri.clone()).expect("deserialize");
        assert_eq!(
            search_struct
                .extra
                .get("query")
                .expect("kind to be deserialized"),
            "test"
        );
        assert_eq!(
            search_struct
                .extra
                .get("kind")
                .expect("kind to be deserialized"),
            "boardgame"
        )
    }

    #[test]
    #[ignore]
    // At present, the implementation here uses Rc<>s, so it can't deserialize to borrows
    // Leaving these tests here as an apiration
    fn test_deserialize_to_borrowed_types() {
        eprintln!("here: {}:{}", file!(), line!());
        check_single_value!(&str, "abc", "abc");
    }

    #[test]
    fn test_parse_single_value() {
        check_single_value!(bool, "true", true);
        check_single_value!(bool, "false", false);
        check_single_value!(i8, "-123", -123);
        check_single_value!(i16, "-123", -123);
        check_single_value!(i32, "-123", -123);
        check_single_value!(i64, "-123", -123);
        check_single_value!(i128, "123", 123);
        check_single_value!(u8, "123", 123);
        check_single_value!(u16, "123", 123);
        check_single_value!(u32, "123", 123);
        check_single_value!(u64, "123", 123);
        check_single_value!(u128, "123", 123);
        check_single_value!(f32, "123", 123.0);
        check_single_value!(f64, "123", 123.0);
        check_single_value!(String, "abc", "abc");
        // check_single_value!(String, "one%20two", "one two"); // percent decoding happens in routing/mod.rs
        // check_single_value!(&str, "one%20two", "one two");
        check_single_value!(char, "a", 'a');

        let url_params = create_url_params(vec![("a", "B")]);
        assert_eq!(
            MyEnum::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            MyEnum::B
        );

        let url_params = create_url_params(vec![("a", "1"), ("b", "2")]);
        let error_kind = i32::deserialize(UriDeserializer {
            varlist: url_params,
        })
        .unwrap_err()
        .kind;
        assert_eq!(
            error_kind,
            ErrorKind::CantFillParameters {
                got: vec!["a".into(), "b".into()],
                expected: 1,
            }
        );
    }

    #[test]
    fn test_parse_explodes() {
        let de = UriDeserializer {
            varlist: vec![(
                "a".into(),
                VarBinding::PathExplode(",".into(), "1,true,abc".into()),
            )],
        };
        assert_eq!(
            Vec::<String>::deserialize(de.clone()).unwrap(),
            vec!["1", "true", "abc"]
        );
        assert_eq!(
            <(i32, bool, String)>::deserialize(de).unwrap(),
            (1, true, "abc".to_owned())
        );

        let de = UriDeserializer {
            varlist: vec![(
                "a".into(),
                VarBinding::QueryExplode(vec!["1".into(), "true".into(), "abc".into()]),
            )],
        };
        assert_eq!(
            Vec::<String>::deserialize(de.clone()).unwrap(),
            vec!["1", "true", "abc"]
        );
        assert_eq!(
            <(i32, bool, String)>::deserialize(de).unwrap(),
            (1, true, "abc".to_owned())
        );
    }

    #[test]
    fn test_parse_seq() {
        let url_params = create_url_params(vec![("a", "1"), ("b", "true"), ("c", "abc")]);
        assert_eq!(
            <(i32, bool, String)>::deserialize(UriDeserializer {
                varlist: url_params.clone()
            })
            .unwrap(),
            (1, true, "abc".to_owned())
        );

        #[derive(Debug, Deserialize, Eq, PartialEq)]
        struct TupleStruct(i32, bool, String);
        assert_eq!(
            TupleStruct::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            TupleStruct(1, true, "abc".to_owned())
        );

        let url_params = create_url_params(vec![("a", "1"), ("b", "2"), ("c", "3")]);
        assert_eq!(
            <Vec<i32>>::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            vec![1, 2, 3]
        );

        let url_params = create_url_params(vec![("a", "c"), ("a", "B")]);
        assert_eq!(
            <Vec<MyEnum>>::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            vec![MyEnum::C, MyEnum::B]
        );
    }

    #[test]
    fn test_parse_seq_tuple_string_string() {
        let url_params = create_url_params(vec![("a", "foo"), ("b", "bar")]);
        assert_eq!(
            <Vec<(String, String)>>::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            vec![
                ("a".to_owned(), "foo".to_owned()),
                ("b".to_owned(), "bar".to_owned())
            ]
        );
    }

    #[test]
    fn test_parse_seq_tuple_string_parse() {
        let url_params = create_url_params(vec![("a", "1"), ("b", "2")]);
        assert_eq!(
            <Vec<(String, u32)>>::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            vec![("a".to_owned(), 1), ("b".to_owned(), 2)]
        );
    }

    #[test]
    fn test_parse_struct() {
        let url_params = create_url_params(vec![("a", "1"), ("b", "true"), ("c", "abc")]);
        assert_eq!(
            Struct::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            Struct {
                c: "abc".to_owned(),
                b: true,
                a: 1,
            }
        );
    }

    #[test]
    fn test_parse_struct_ignoring_additional_fields() {
        let url_params = create_url_params(vec![
            ("a", "1"),
            ("b", "true"),
            ("c", "abc"),
            ("d", "false"),
        ]);
        assert_eq!(
            Struct::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            Struct {
                c: "abc".to_owned(),
                b: true,
                a: 1,
            }
        );
    }

    #[test]
    fn test_parse_tuple_ignoring_additional_fields() {
        let url_params = create_url_params(vec![
            ("a", "abc"),
            ("b", "true"),
            ("c", "1"),
            ("d", "false"),
        ]);
        assert_eq!(
            <(String, bool, u32)>::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            ("abc".into(), true, 1)
        );
    }

    #[test]
    fn test_parse_map() {
        let url_params = create_url_params(vec![("a", "1"), ("b", "true"), ("c", "abc")]);
        assert_eq!(
            <HashMap<String, String>>::deserialize(UriDeserializer {
                varlist: url_params
            })
            .unwrap(),
            [("a", "1"), ("b", "true"), ("c", "abc")]
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        );
    }

    macro_rules! test_parse_error {
        (
            $params:expr,
            $ty:ty,
            $expected_error_kind:expr $(,)?
        ) => {
            let url_params = create_url_params($params);
            let actual_error_kind = <$ty>::deserialize(UriDeserializer {
                varlist: url_params,
            })
            .unwrap_err()
            .kind;
            assert_eq!(actual_error_kind, $expected_error_kind);
        };
    }

    #[test]
    fn test_wrong_number_of_parameters_error() {
        test_parse_error!(
            vec![("a", "1")],
            (u32, u32),
            ErrorKind::CantFillParameters {
                got: vec!["a".into()],
                expected: 2,
            }
        );
    }

    #[test]
    fn test_parse_error_at_key_error() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Params {
            a: u32,
        }
        test_parse_error!(
            vec![("a", "false")],
            Params,
            ErrorKind::ParseErrorAtKey {
                key: "a".to_owned(),
                value: "false".to_owned(),
                expected_type: "u32",
            }
        );
    }

    #[test]
    fn test_parse_error_at_key_error_multiple() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Params {
            a: u32,
            b: u32,
        }
        test_parse_error!(
            vec![("a", "false")],
            Params,
            ErrorKind::ParseErrorAtKey {
                key: "a".to_owned(),
                value: "false".to_owned(),
                expected_type: "u32",
            }
        );
    }

    #[test]
    fn test_parse_error_at_index_error() {
        test_parse_error!(
            vec![("a", "false"), ("b", "true")],
            (bool, u32),
            ErrorKind::ParseErrorAtIndex {
                index: 1,
                value: "true".to_owned(),
                expected_type: "u32",
            }
        );
    }

    #[test]
    fn test_parse_error_error() {
        test_parse_error!(
            vec![("a", "false")],
            u32,
            ErrorKind::ParseError {
                value: "false".to_owned(),
                expected_type: "u32",
            }
        );
    }

    #[test]
    fn test_unsupported_type_error_nested_data_structure() {
        test_parse_error!(
            vec![("a", "false")],
            Vec<Vec<u32>>,
            ErrorKind::UnsupportedType {
                name: "alloc::vec::Vec<u32>",
            }
        );
    }

    #[test]
    fn test_parse_seq_tuple_unsupported_key_type() {
        test_parse_error!(
            vec![("a", "false")],
            Vec<(u32, String)>,
            ErrorKind::Message("Unexpected key type".to_owned())
        );
    }

    #[test]
    fn test_parse_seq_wrong_tuple_length() {
        test_parse_error!(
            vec![("a", "false")],
            Vec<(String, String, String)>,
            ErrorKind::UnsupportedType {
                name: "(alloc::string::String, alloc::string::String, alloc::string::String)",
            }
        );
    }

    #[test]
    fn test_parse_seq_seq() {
        test_parse_error!(
            vec![("a", "false")],
            Vec<Vec<String>>,
            ErrorKind::UnsupportedType {
                name: "alloc::vec::Vec<alloc::string::String>",
            }
        );
    }
}
