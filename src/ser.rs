use std::{
    collections::{hash_map, HashMap},
    fmt::Display,
    rc::Rc,
    str,
    sync::LazyLock,
};

use serde::{
    ser::{
        self, Impossible, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
        SerializeTuple, SerializeTupleStruct, SerializeTupleVariant,
    },
    Serialize,
};

use crate::{parser::VarSpec, FillPolicy, Parsed, VarMod};

use super::parser;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("custom error: {0}")]
    CustomError(String),
    #[error("fewer parameters provided ({0}) than fields in template {1}")]
    MissingPositionalFields(usize, usize),
    #[error("more parameters provided ({0}) than fields in template {1}")]
    ExtraPositionalFields(usize, usize),
    #[error("one or more parameters provided than there are fields in template")]
    OverrunPositionalField, // when this happens, we don't have sizing contexts
    #[error("no value provided for variable: {0}")]
    MissingNamedVariable(String),
    #[error("bindings provided don't match expression - this is a Mattak error")]
    BindingsDontMatchExpression,
    #[error("cannot serialize a {0} as a key")]
    InvalidKeyType(String),
    #[error("serde provided value before key")]
    SerializationStateError,
    #[error("unexpected field: {0}")]
    ExtraNamedField(String),
    #[error("couldn't serialize non-utf8 bytes: {0}")]
    NotUTF8(std::str::Utf8Error),
    #[error("cannot serialize {0} types in this position")]
    InvalidComplexType(String),
    #[error("Complex {0} value provided for template variable {1}")]
    PrefixedComplexValue(String, String),
}

impl ser::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: Display,
    {
        Error::CustomError(msg.to_string())
    }
}

macro_rules! demark {
    ($name:ident,$mark:literal) => {
        const $name: LazyLock<Rc<str>> = LazyLock::new(|| Rc::from($mark));
    };
}
demark!(EMPTY, "");
demark!(OPEN_BRACE, "{");
demark!(CLOSE_BRACE, "}");
demark!(COMMA, ",");
demark!(EQUALS, "=");
demark!(DOT, ".");
demark!(SEMI, ";");
demark!(AND, "&");
demark!(QMARK, "?");
demark!(SLASH, "/");
demark!(OCTOTHORPE, "#");

#[derive(Debug)]
enum VarBinding {
    Absent,  // serialize value doesn't provide
    Omitted, // serialize value provides None
    Scalar(Rc<str>),
    List(Vec<Rc<str>>),
    Map(Vec<(Rc<str>, Option<Rc<str>>)>),
}

impl VarSpec {
    fn to_str(&self) -> Rc<str> {
        use VarMod::*;
        match self.modifier {
            Prefix(n) => Rc::from(format!("{}:{n}", self.varname)),
            Explode => Rc::from(format!("{}*", self.varname)),
            None => Rc::from(self.varname.clone()),
        }
    }
}

fn mapexprs(
    template: Parsed,
    policy: FillPolicy,
    bindings: HashMap<Rc<str>, VarBinding>,
) -> Result<Vec<Rc<str>>, Error> {
    template
        .parts_iter()
        .map(|part| {
            use crate::Part::*;
            match part {
                Lit(s) => Ok(vec![Rc::<str>::from(s.clone())]),
                SegPathVar(expression) | SegPathRest(expression) | Expression(expression) => {
                    mapvars(expression, policy, &bindings)
                }
                SegVar(expression) | SegRest(expression) => {
                    mapvars(expression, policy, &bindings).map(|mut vec| {
                        vec.insert(0, SLASH.clone()); // XXX perf O(n)!
                        vec
                    })
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.into_iter().flatten().collect())
}

fn mapvars(
    expression: &parser::Expression,
    policy: FillPolicy,
    bindings: &HashMap<Rc<str>, VarBinding>,
) -> Result<Vec<Rc<str>>, Error> {
    use parser::Op::*;
    // The process here is that we need to break an Expression
    // into "stripes": literal parts where it's resolved
    // and new "unbound" expressions where it isn't.
    // If the FillPolicy forbids missing variables,
    // we can return an error as soon as we hit one, though
    let mut resolved = vec![];

    let mut joiner: Rc<str>;
    let mut specs = expression.varspecs.iter().peekable();
    while specs.peek().is_some() {
        joiner = prefix_for(expression);
        while let Some(varspec) = specs.peek() {
            let name = Rc::from(varspec.varname.clone());
            let vmod = varspec.modifier.clone();
            let vb = bindings
                .get(&name)
                .ok_or(Error::BindingsDontMatchExpression)?;
            match vb {
                VarBinding::Absent => {
                    use FillPolicy::*;
                    match policy {
                        NoMissing | Strict => {
                            return Err(Error::MissingNamedVariable(varspec.varname.clone()))
                        }
                        NoExtra | Relaxed => break,
                        DropMissing => {
                            specs.next();
                        }
                    }
                }
                VarBinding::Omitted => {
                    specs.next();
                }
                VarBinding::Scalar(val) => {
                    specs.next();
                    resolved.push(joiner.clone());
                    if matches!(expression.operator, PathParam | Query | QueryCont) {
                        resolved.push(name.clone());
                        if !(matches!(expression.operator, PathParam) && val.len() == 0) {
                            resolved.push(EQUALS.clone());
                        }
                    }
                    if let VarMod::Prefix(n) = vmod {
                        // This trainwreck brought to you by the Unicode Consortium
                        let src = if let Some((idx, _char)) = val.char_indices().nth(n.into()) {
                            let (prefix, _rest) = val.split_at(idx);
                            prefix
                        } else {
                            &val
                        };
                        escape_reserved(expression.operator, &mut resolved, &src)
                    } else {
                        escape_reserved(expression.operator, &mut resolved, &val)
                    }
                    joiner = separator_for(expression);
                }
                VarBinding::List(items) => {
                    specs.next();
                    if !(matches!(expression.operator, Label) && items.len() == 0) {
                        resolved.push(joiner.clone());
                    }
                    match (vmod, expression.operator) {
                        (VarMod::Prefix(_), _) => {
                            return Err(Error::PrefixedComplexValue(
                                "map".into(),
                                name.as_ref().into(),
                            ))
                        }
                        (VarMod::None, PathParam | Query | QueryCont) => {
                            resolved.push(name.clone());
                            resolved.push(EQUALS.clone());
                            expand_list(items, expression, &COMMA, &mut resolved);
                        }
                        (VarMod::None, _) | (VarMod::Explode, Simple | Reserved | Fragment) => {
                            expand_list(items, expression, &COMMA, &mut resolved);
                        }
                        (VarMod::Explode, Label) => {
                            expand_list(items, expression, &DOT, &mut resolved);
                        }
                        (VarMod::Explode, Path) => {
                            expand_list(items, expression, &SLASH, &mut resolved);
                        }
                        (VarMod::Explode, PathParam) => {
                            let withname: Vec<_> = items
                                .iter()
                                .map(|item| (name.clone(), Some(item.clone())))
                                .collect();
                            expand_map(expression, &SEMI, &EQUALS, &mut resolved, &withname);
                        }
                        (VarMod::Explode, Query | QueryCont) => {
                            let withname: Vec<_> = items
                                .iter()
                                .map(|item| (name.clone(), Some(item.clone())))
                                .collect();
                            expand_map(expression, &AND, &EQUALS, &mut resolved, &withname);
                        }
                    }
                    joiner = separator_for(expression);
                }
                VarBinding::Map(items) => {
                    specs.next();
                    resolved.push(joiner.clone());
                    match (vmod, expression.operator) {
                        (VarMod::Prefix(_), _) => {
                            return Err(Error::PrefixedComplexValue(
                                "map".into(),
                                name.as_ref().into(),
                            ))
                        }
                        (VarMod::None, PathParam | Query | QueryCont) => {
                            resolved.push(name.clone());
                            resolved.push(EQUALS.clone());
                            expand_map(expression, &COMMA, &COMMA, &mut resolved, items);
                        }

                        (VarMod::None, _) => {
                            expand_map(expression, &COMMA, &COMMA, &mut resolved, items);
                        }

                        (VarMod::Explode, Simple | Reserved | Fragment) => {
                            expand_map(expression, &COMMA, &EQUALS, &mut resolved, items);
                        }
                        (VarMod::Explode, Label) => {
                            expand_map(expression, &DOT, &EQUALS, &mut resolved, items);
                        }
                        (VarMod::Explode, Path) => {
                            expand_map(expression, &SLASH, &EQUALS, &mut resolved, items);
                        }
                        (VarMod::Explode, PathParam) => {
                            expand_map(expression, &SEMI, &EQUALS, &mut resolved, items);
                        }
                        (VarMod::Explode, Query | QueryCont) => {
                            expand_map(expression, &AND, &EQUALS, &mut resolved, items);
                        }
                    }
                    joiner = separator_for(expression);
                }
            }
        }

        if specs.peek().is_some() {
            match expression.operator {
                Query if resolved.len() == 0 => {
                    resolved.push(OPEN_BRACE.clone());
                    resolved.push(QMARK.clone());
                }
                Fragment if resolved.len() == 0 => {
                    resolved.push(OPEN_BRACE.clone());
                    resolved.push(OCTOTHORPE.clone());
                }
                Simple | Reserved | Fragment => {
                    resolved.push(COMMA.clone());
                    resolved.push(OPEN_BRACE.clone());
                }
                Label => {
                    resolved.push(OPEN_BRACE.clone());
                    resolved.push(DOT.clone());
                }
                Path => {
                    resolved.push(OPEN_BRACE.clone());
                    resolved.push(SLASH.clone());
                }
                PathParam => {
                    resolved.push(OPEN_BRACE.clone());
                    resolved.push(SEMI.clone());
                }
                Query | QueryCont => {
                    resolved.push(OPEN_BRACE.clone());
                    resolved.push(AND.clone());
                }
            }
            joiner = EMPTY.clone();
        }

        while let Some(varspec) = specs.peek() {
            let vb = bindings
                .get(&Rc::from(varspec.varname.clone()))
                .ok_or(Error::BindingsDontMatchExpression)?;

            if matches!(vb, VarBinding::Absent) {
                resolved.push(joiner);
                resolved.push(varspec.to_str());
                specs.next();
                joiner = COMMA.clone();
            } else {
                resolved.push(CLOSE_BRACE.clone());
                break;
            }
        }
    }
    Ok(resolved)
}

fn prefix_for(expression: &parser::Expression) -> Rc<str> {
    use parser::Op::*;
    match expression.operator {
        Simple | Reserved => EMPTY.clone(),
        Fragment => OCTOTHORPE.clone(),
        Label => DOT.clone(),
        Path => SLASH.clone(),
        PathParam => SEMI.clone(),
        Query => QMARK.clone(),
        QueryCont => AND.clone(),
    }
}

fn separator_for(expression: &parser::Expression) -> Rc<str> {
    use parser::Op::*;
    match expression.operator {
        Simple | Reserved | Fragment => COMMA.clone(),
        Label => DOT.clone(),
        Path => SLASH.clone(),
        PathParam => SEMI.clone(),
        Query | QueryCont => AND.clone(),
    }
}

fn expand_list(
    items: &Vec<Rc<str>>,
    expression: &parser::Expression,
    separator: &Rc<str>,
    resolved: &mut Vec<Rc<str>>,
) {
    let mut inter = false;
    items.iter().for_each(|val| {
        if inter {
            resolved.push(separator.clone());
        }
        escape_reserved(expression.operator, resolved, val);
        inter = true;
    })
}

fn expand_map<'a>(
    expression: &parser::Expression,
    separator: &Rc<str>, // between pairs
    joiner: &Rc<str>,    // within pairs
    resolved: &mut Vec<Rc<str>>,
    items: impl IntoIterator<Item = &'a (Rc<str>, Option<Rc<str>>)>,
) {
    let mut inter = false;
    items.into_iter().for_each(|(k, v)| {
        if inter {
            resolved.push(separator.clone());
        }
        resolved.push(k.clone());
        resolved.push(joiner.clone());
        if let Some(val) = v {
            escape_reserved(expression.operator, resolved, &val);
        }
        inter = true;
    })
}

/*
Inspiration taken from the urlencoding crate -

Needed to be able to select the reserved character set,
to always escape % unless it preceeds two hex digits.

Decided to optimize for time over space with encoding lookups.
*/

fn escape_reserved(op: parser::Op, result: &mut Vec<Rc<str>>, val: &str) {
    let mut data = val.as_bytes();
    let mut pushed = false;
    loop {
        // Fast path to skip over safe chars at the beginning of the remaining string
        let ascii_len = data
            .iter()
            .take_while(|&&c| allowed_character(c, op))
            .count();

        let (safe, rest) = if ascii_len >= data.len() {
            if !pushed {
                result.push(Rc::from(val));
                return;
            }
            (data, &[][..]) // redundant to optimize out a panic in split_at
        } else {
            data.split_at(ascii_len)
        };
        pushed = true;
        if !safe.is_empty() {
            result.push(Rc::from(str::from_utf8(safe).expect(
                "safe string is entirely allowed_character, so should be utf8",
            )));
        }

        match rest.split_first() {
            Some((byte, rest)) => {
                if *byte == b'%'
                    && matches!(op, parser::Op::Fragment | parser::Op::Reserved)
                    && rest.len() >= 2
                {
                    match rest.split_at(2) {
                        (
                            hexdigs @
                            [b'A'..=b'F' | b'a'..=b'f' | b'0'..=b'9', b'A'..=b'F' | b'a'..=b'f' | b'0'..=b'9'],
                            pctrest,
                        ) => {
                            result.push(Rc::from(
                                str::from_utf8(&[b'%', hexdigs[0], hexdigs[1]])
                                    .expect("gotta be okay: %ff"),
                            ));
                            data = pctrest;
                        }
                        _ => {
                            result.push(percent_encode(*byte));
                            data = rest;
                        }
                    }
                } else {
                    result.push(percent_encode(*byte));
                    data = rest;
                }
            }
            None => return,
        };
    }
}

#[inline]
fn allowed_character(c: u8, op: parser::Op) -> bool {
    use parser::Op::*;
    match op {
        Reserved | Fragment => {
            // unreserved / reserved / pct-encoding
            // unreserved     =  ALPHA / DIGIT / "-" / "." / "_" / "~"
            // reserved       =  gen-delims / sub-delims
            // gen-delims     =  ":" / "/" / "?" / "#" / "[" / "]" / "@"
            // sub-delims     =  "!" / "$" / "&" / "'" / "(" / ")"
            //                /  "*" / "+" / "," / ";" / "="
            // pct-encoded    =  "%" HEXDIG HEXDIG
            matches!(c, b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' |
                b'-' | b'.' | b'_' | b'~' |
                b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'@' |
                b'!' | b'$' | b'&' | b'\'' | b'(' | b')' |
                b'*' | b'+' | b',' | b';' | b'=')
        }
        _ => {
            // unreserved
            matches!(c, b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' |  b'-' | b'.' | b'_' | b'~')
        }
    }
}

// we burn ~2Ki on every possible encoding
// which would be unacceptable if we were writing a 80s era game cart
const ENCODINGS: LazyLock<Vec<Rc<str>>> = LazyLock::new(|| {
    (0..=u8::MAX)
        .into_iter()
        .map(|byte: u8| {
            let enc_slice = &[b'%', to_hex_digit(byte >> 4), to_hex_digit(byte & 15)];
            Rc::from(str::from_utf8(enc_slice).expect("limited bytes to be utf8"))
        })
        .collect()
});

fn percent_encode(byte: u8) -> Rc<str> {
    ENCODINGS[byte as usize].clone()
}

#[inline]
fn to_hex_digit(digit: u8) -> u8 {
    match digit {
        0..=9 => b'0' + digit,
        10..=255 => b'A' - 10 + digit,
    }
}

pub fn fill(parsed: &Parsed, policy: FillPolicy, context: impl Serialize) -> Result<String, Error> {
    let mut serializer = Serializer {
        template: parsed.clone(),
        policy,
    };
    context.serialize(&mut serializer)
}

macro_rules! serializer_base_types {
    () => {
        type Ok = VarBinding;
        type Error = Error;
    };
}

macro_rules! serializer_complex_types {
    ($seq:ty,$map:ty) => {
        type SerializeSeq = $seq;
        type SerializeTuple = $seq;
        type SerializeTupleStruct = $seq;
        type SerializeTupleVariant = $seq;
        type SerializeMap = $map;
        type SerializeStruct = $map;
        type SerializeStructVariant = $map;
    };
}
macro_rules! serializer_types {
    ($seq:ty,$map:ty) => {
        serializer_base_types!();
        serializer_complex_types!($seq, $map);
    };
}

macro_rules! serialize_single_value {
    ($trait_fn:ident, $ty:ty) => {
        fn $trait_fn(self, v: $ty) -> Result<Self::Ok, Self::Error> {
            let mut seq = self.serialize_tuple(1)?;
            SerializeSeq::serialize_element(&mut seq, &v)?;
            SerializeSeq::end(seq)
        }
    };
}

struct Serializer {
    template: Parsed,
    policy: FillPolicy,
}

impl<'a> serde::Serializer for &'a mut Serializer {
    type Ok = String;

    type Error = Error;

    serializer_complex_types!(PositionSerializer, NamedSerializer);
    serialize_single_value!(serialize_bool, bool);
    serialize_single_value!(serialize_i8, i8);
    serialize_single_value!(serialize_i16, i16);
    serialize_single_value!(serialize_i32, i32);
    serialize_single_value!(serialize_i64, i64);
    serialize_single_value!(serialize_u8, u8);
    serialize_single_value!(serialize_u16, u16);
    serialize_single_value!(serialize_u32, u32);
    serialize_single_value!(serialize_u64, u64);
    serialize_single_value!(serialize_f32, f32);
    serialize_single_value!(serialize_f64, f64);
    serialize_single_value!(serialize_char, char);
    serialize_single_value!(serialize_str, &str);
    serialize_single_value!(serialize_bytes, &[u8]);

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let mut seq = self.serialize_tuple(1)?;
        SerializeSeq::serialize_element(&mut seq, value)?;
        SerializeSeq::end(seq)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        let seq = self.serialize_tuple(0)?;
        SerializeSeq::end(seq)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let mut seq = self.serialize_tuple(1)?;
        SerializeSeq::serialize_element(&mut seq, &value)?;
        SerializeSeq::end(seq)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let mut seq = self.serialize_tuple(1)?;
        SerializeSeq::serialize_element(&mut seq, &value)?;
        SerializeSeq::end(seq)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        PositionSerializer::build(self.policy, len, self.template.clone())
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        PositionSerializer::build(self.policy, Some(len), self.template.clone())
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        PositionSerializer::build(self.policy, Some(len), self.template.clone())
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        PositionSerializer::build(self.policy, Some(len), self.template.clone())
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        NamedSerializer::build(self.policy, len, self.template.clone())
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        NamedSerializer::build(self.policy, Some(len), self.template.clone())
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        NamedSerializer::build(self.policy, Some(len), self.template.clone())
    }
}

// serialize the template based on variable first-appearance position
struct PositionSerializer {
    template: Parsed, //XXX needed?
    policy: FillPolicy,
    pos: Vec<Rc<str>>,
    vals: HashMap<Rc<str>, VarBinding>,
}

impl PositionSerializer {
    fn build(policy: FillPolicy, len: Option<usize>, template: Parsed) -> Result<Self, Error> {
        let mut pos = match len {
            None => vec![],
            Some(size) => Vec::with_capacity(size),
        };
        let mut vals = match len {
            None => HashMap::new(),
            Some(size) => HashMap::with_capacity(size),
        };

        template.parts_iter().for_each(|part| {
            if let Some(exp) = part.expression() {
                for vs in &exp.varspecs {
                    let name = Rc::<str>::from(vs.varname.clone());
                    match vals.entry(name.clone()) {
                        hash_map::Entry::Occupied(_) => (),
                        hash_map::Entry::Vacant(vacant_entry) => {
                            pos.push(name);
                            vacant_entry.insert(VarBinding::Absent);
                        }
                    }
                }
            }
        });

        if let Some(l) = len {
            use FillPolicy::*;
            match policy {
                Relaxed => (),
                NoMissing | Strict if l < pos.len() => {
                    return Err(Error::MissingPositionalFields(l, pos.len()))
                }
                NoExtra | Strict if l > pos.len() => {
                    return Err(Error::ExtraPositionalFields(l, pos.len()))
                }
                _ => (),
            }
        }

        Ok(Self {
            template,
            policy,
            pos,
            vals,
        })
    }
}

macro_rules! pos_impl {
    ($trait:ident, $item_fn:ident) => {
        impl<'a> $trait for PositionSerializer {
            type Ok = String;

            type Error = Error;

            fn $item_fn<T>(&mut self, value: &T) -> Result<(), Self::Error>
            where
                T: ?Sized + Serialize,
            {
                let name = if let Some(n) = self.pos.pop() {
                    n
                } else {
                    use FillPolicy::*;
                    return match self.policy {
                        Relaxed | DropMissing | NoMissing => Ok(()), // XXX DropMissing ?
                        NoExtra | Strict => Err(Error::OverrunPositionalField),
                    };
                };

                self.vals
                    .insert(name, value.serialize(VariableSerializer {})?);
                Ok(())
            }

            fn end(self) -> Result<Self::Ok, Self::Error> {
                mapexprs(self.template, self.policy, self.vals)
                    .map(|v| v.iter().map(|rc| rc.as_ref()).collect())
            }
        }
    };
}

pos_impl!(SerializeSeq, serialize_element);
pos_impl!(SerializeTuple, serialize_element);
pos_impl!(SerializeTupleStruct, serialize_field);
pos_impl!(SerializeTupleVariant, serialize_field);

struct NamedSerializer {
    template: Parsed,
    policy: FillPolicy,
    key: Option<Rc<str>>,
    vals: HashMap<Rc<str>, VarBinding>,
}

impl NamedSerializer {
    fn build(policy: FillPolicy, len: Option<usize>, template: Parsed) -> Result<Self, Error> {
        let mut vals = match len {
            None => HashMap::new(),
            Some(size) => HashMap::with_capacity(size),
        };

        let mut field_count = 0; // saves allocating the vector...

        template.parts_iter().for_each(|part| {
            if let Some(exp) = part.expression() {
                for vs in &exp.varspecs {
                    let name = Rc::<str>::from(vs.varname.clone());
                    match vals.entry(name) {
                        hash_map::Entry::Occupied(_) => (),
                        hash_map::Entry::Vacant(vacant_entry) => {
                            field_count += 1;
                            vacant_entry.insert(VarBinding::Absent);
                        }
                    }
                }
            }
        });

        if let Some(l) = len {
            use FillPolicy::*;
            match policy {
                Relaxed => (),
                NoMissing | Strict if l < field_count => {
                    return Err(Error::MissingPositionalFields(l, field_count))
                }
                NoExtra | Strict if l > field_count => {
                    return Err(Error::ExtraPositionalFields(l, field_count))
                }
                _ => (),
            }
        }

        Ok(Self {
            key: None,
            template,
            policy,
            vals,
        })
    }

    fn insert_value<T: ?Sized + Serialize>(
        &mut self,
        name: Rc<str>,
        value: &T,
    ) -> Result<(), Error> {
        use FillPolicy::*;
        match self.vals.entry(name.clone()) {
            hash_map::Entry::Occupied(mut b) => b.insert(value.serialize(VariableSerializer {})?),
            hash_map::Entry::Vacant(_) => match self.policy {
                Relaxed | DropMissing | NoMissing => return Ok(()),
                NoExtra | Strict => return Err(Error::ExtraNamedField(name.as_ref().into())),
            },
        };
        Ok(())
    }
}

impl<'a> SerializeMap for NamedSerializer {
    type Ok = String;

    type Error = Error;

    fn serialize_entry<K, V>(&mut self, key: &K, value: &V) -> Result<(), Self::Error>
    where
        K: ?Sized + Serialize,
        V: ?Sized + Serialize,
    {
        let name = key.serialize(KeySerializer {})?;
        self.insert_value(name, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        mapexprs(self.template, self.policy, self.vals)
            .map(|v| v.iter().map(|rc| rc.as_ref()).collect())
    }

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let _ = self.key.insert(key.serialize(KeySerializer {})?);
        Ok(())
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let name = if let Some(n) = self.key.take() {
            n
        } else {
            return Err(Error::SerializationStateError);
        };
        self.insert_value(name, value)
    }
}

impl<'a> SerializeStruct for NamedSerializer {
    type Ok = String;

    type Error = Error;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let name = Rc::<str>::from(key);
        self.insert_value(name, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        mapexprs(self.template, self.policy, self.vals)
            .map(|v| v.iter().map(|rc| rc.as_ref()).collect())
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        let _ = key;
        Ok(())
    }
}

impl<'a> SerializeStructVariant for NamedSerializer {
    type Ok = String;

    type Error = Error;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let name = Rc::<str>::from(key);
        self.insert_value(name, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        mapexprs(self.template, self.policy, self.vals)
            .map(|v| v.iter().map(|rc| rc.as_ref()).collect())
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        let _ = key;
        Ok(())
    }
}

macro_rules! render_single_value {
    ($trait_fn:ident, $ty:ty) => {
        fn $trait_fn(self, v: $ty) -> Result<Self::Ok, Self::Error> {
            Ok(VarBinding::Scalar(Rc::from(format!("{v}"))))
        }
    };
}

macro_rules! list_item {
    ($trait_fn:ident) => {
        fn $trait_fn<T>(&mut self, value: &T) -> Result<(), Self::Error>
        where
            T: ?Sized + Serialize,
        {
            if let Some(v) = value.serialize(ValueSerializer {})? {
                self.output.push(v);
            }
            Ok(())
        }
    };
}

macro_rules! list_end {
    () => {
        fn end(self) -> Result<Self::Ok, Self::Error> {
            Ok(VarBinding::List(self.output))
        }
    };
}

macro_rules! map_end {
    () => {
        fn end(self) -> Result<Self::Ok, Self::Error> {
            Ok(VarBinding::Map(self.output))
        }
    };
}

#[derive(Debug)]
struct VariableSerializer {}

impl serde::Serializer for VariableSerializer {
    serializer_types!(ListSerializer, AssocSerializer);

    render_single_value!(serialize_bool, bool);
    render_single_value!(serialize_i8, i8);
    render_single_value!(serialize_i16, i16);
    render_single_value!(serialize_i32, i32);
    render_single_value!(serialize_i64, i64);
    render_single_value!(serialize_u8, u8);
    render_single_value!(serialize_u16, u16);
    render_single_value!(serialize_u32, u32);
    render_single_value!(serialize_u64, u64);
    render_single_value!(serialize_f32, f32);
    render_single_value!(serialize_f64, f64);
    render_single_value!(serialize_char, char);

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        Ok(VarBinding::Scalar(Rc::from(v)))
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(VarBinding::Scalar(Rc::from(
            str::from_utf8(v).map_err(Error::NotUTF8)?,
        )))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Ok(VarBinding::Omitted)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(VarBinding::Omitted)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(VarBinding::Omitted)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(VarBinding::Omitted)
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(ListSerializer::new(len))
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(ListSerializer::new(Some(len)))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(ListSerializer::new(Some(len)))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(ListSerializer::new(Some(len)))
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(AssocSerializer::new(len))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(AssocSerializer::new(Some(len)))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(AssocSerializer::new(Some(len)))
    }
}

#[derive(Debug)]
struct ListSerializer {
    output: Vec<Rc<str>>,
}

impl ListSerializer {
    fn new(size: Option<usize>) -> Self {
        Self {
            output: match size {
                Some(s) => Vec::with_capacity(s),
                None => Vec::new(),
            },
        }
    }
}

impl SerializeSeq for ListSerializer {
    serializer_base_types!();

    list_item!(serialize_element);

    list_end!();
}
impl SerializeTuple for ListSerializer {
    serializer_base_types!();

    list_item!(serialize_element);
    list_end!();
}
impl SerializeTupleStruct for ListSerializer {
    serializer_base_types!();
    list_item!(serialize_field);
    list_end!();
}
impl SerializeTupleVariant for ListSerializer {
    serializer_base_types!();
    list_item!(serialize_field);
    list_end!();
}

struct AssocSerializer {
    key: Option<Rc<str>>,
    output: Vec<(Rc<str>, Option<Rc<str>>)>,
}

impl AssocSerializer {
    fn new(size: Option<usize>) -> Self {
        Self {
            key: None,
            output: match size {
                Some(s) => Vec::with_capacity(s), // *4?
                None => Vec::new(),
            },
        }
    }

    fn insert_value<T>(&mut self, name: Rc<str>, value: &T) -> Result<(), Error>
    where
        T: ?Sized + Serialize,
    {
        let val = value.serialize(ValueSerializer {})?;
        self.output.push((name, val));
        Ok(())
    }
}

impl SerializeMap for AssocSerializer {
    serializer_base_types!();

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let _ = self.key.insert(key.serialize(KeySerializer {})?);
        Ok(())
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let name = if let Some(n) = self.key.take() {
            n
        } else {
            return Err(Error::SerializationStateError);
        };
        self.insert_value(name, value)
    }

    map_end!();
}
impl SerializeStruct for AssocSerializer {
    serializer_base_types!();

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let k = key.serialize(KeySerializer {})?;
        self.insert_value(k, value)
    }

    map_end!();
}
impl SerializeStructVariant for AssocSerializer {
    serializer_base_types!();

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let k = key.serialize(KeySerializer {})?;
        self.insert_value(k, value)
    }

    map_end!();
}

struct KeySerializer {}

macro_rules! serializer_simple_types {
    ($ok:ty) => {
        type Ok = $ok;
        type Error = Error;

        serializer_complex_types!(
            Impossible<$ok, Error>,
            Impossible<$ok, Error>
        );
    };
}
macro_rules! format_key {
    ($trait_fn:ident, $ty:ty) => {
        fn $trait_fn(self, v: $ty) -> Result<Self::Ok, Self::Error> {
            Ok(Rc::from(format!("{}", v)))
        }
    };
}

impl serde::Serializer for KeySerializer {
    serializer_simple_types!(Rc<str>);

    format_key!(serialize_bool, bool);
    format_key!(serialize_i8, i8);
    format_key!(serialize_i16, i16);
    format_key!(serialize_i32, i32);
    format_key!(serialize_i64, i64);
    format_key!(serialize_u8, u8);
    format_key!(serialize_u16, u16);
    format_key!(serialize_u32, u32);
    format_key!(serialize_u64, u64);
    format_key!(serialize_f32, f32);
    format_key!(serialize_f64, f64);
    format_key!(serialize_char, char);

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        Ok(Rc::from(v))
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(Rc::from(str::from_utf8(v).map_err(Error::NotUTF8)?))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(Error::InvalidKeyType("none".into()))
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(Error::InvalidKeyType("unit".into()))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(Error::InvalidKeyType("unit struct".into()))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Err(Error::InvalidKeyType("unit variant".into()))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(Error::InvalidComplexType("seq".into()))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(Error::InvalidComplexType("tuple".into()))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(Error::InvalidComplexType("tuple struct".into()))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(Error::InvalidComplexType("tuple variant".into()))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(Error::InvalidComplexType("map".into()))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Err(Error::InvalidComplexType("struct".into()))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(Error::InvalidComplexType("struct variant".into()))
    }
}

struct ValueSerializer {}

macro_rules! format_simple_value {
    ($trait_fn:ident, $ty:ty) => {
        fn $trait_fn(self, v: $ty) -> Result<Self::Ok, Self::Error> {
            Ok(Some(Rc::from(format!("{}", v))))
        }
    };
}

impl serde::Serializer for ValueSerializer {
    serializer_simple_types!(Option<Rc<str>>);

    format_simple_value!(serialize_bool, bool);
    format_simple_value!(serialize_i8, i8);
    format_simple_value!(serialize_i16, i16);
    format_simple_value!(serialize_i32, i32);
    format_simple_value!(serialize_i64, i64);
    format_simple_value!(serialize_u8, u8);
    format_simple_value!(serialize_u16, u16);
    format_simple_value!(serialize_u32, u32);
    format_simple_value!(serialize_u64, u64);
    format_simple_value!(serialize_f32, f32);
    format_simple_value!(serialize_f64, f64);
    format_simple_value!(serialize_char, char);

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        Ok(Some(Rc::from(v)))
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        Ok(Some(Rc::from(str::from_utf8(v).map_err(Error::NotUTF8)?)))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Ok(None)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(None)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(None)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(None)
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(Error::InvalidComplexType("seq".into()))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(Error::InvalidComplexType("tuple".into()))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(Error::InvalidComplexType("tuple struct".into()))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(Error::InvalidComplexType("tuple variant".into()))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(Error::InvalidComplexType("map".into()))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Err(Error::InvalidComplexType("struct".into()))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(Error::InvalidComplexType("struct variant".into()))
    }
}
#[cfg(test)]
mod test {
    use crate::{parsed, RouteTemplateString};

    use super::*;

    // Some needed tests:
    //
    // examples from RFC6570 S1.2
    // omitted values; not undef: null, but completely unmentioned
    // different if NoMissing is the policy, but otherwise same behavior
    // test other fill policies
    //
    // Serialize from other data formats - structs and tuples especially

    fn test_template(template: &str, expansion: &str) {
        // nota bene: our templates are restricted from RFC 6570, such that
        // bare {...} isn't allowed (because matchit would get sad)
        test_path_template(&format!("/{template}"), &format!("/{expansion}"))
    }

    #[derive(Serialize)]
    struct TMap<'a> {
        semi: &'a str,
        dot: &'a str,
        comma: &'a str,
    }

    fn test_path_template(template: &str, expansion: &str) {
        let mut definitions = HashMap::new();
        #[derive(Serialize)]
        enum Var<'a> {
            List(Vec<&'a str>),
            Str(&'a str),
            Map(TMap<'a>),
            Null,
        }
        use Var::*;

        let keys = TMap {
            semi: ";",
            dot: ".",
            comma: ",",
        };

        definitions.insert("count", List(vec!["one", "two", "three"]));
        definitions.insert("dom", List(vec!["example", "com"]));
        definitions.insert("dub", Str("me/too"));
        definitions.insert("hello", Str("Hello World!"));
        definitions.insert("half", Str("50%"));
        definitions.insert("var", Str("value"));
        definitions.insert("who", Str("fred"));
        definitions.insert("base", Str("http://example.com/home/"));
        definitions.insert("path", Str("/foo/bar"));
        definitions.insert("list", List(vec!["red", "green", "blue"]));
        definitions.insert("keys", Map(keys));
        definitions.insert("v", Str("6"));
        definitions.insert("x", Str("1024"));
        definitions.insert("y", Str("768"));
        definitions.insert("empty", Str(""));
        definitions.insert("empty_keys", List(vec![])); // XXX probably meant to be and empty map
        definitions.insert("undef", Null);

        let rt = RouteTemplateString(template.into(), vec![]);

        let parsed = parsed(rt).expect("test template to parse");

        let mut serializer = Serializer {
            template: parsed,
            policy: FillPolicy::DropMissing,
        };
        let serialized = definitions
            .serialize(&mut serializer)
            .expect("test template to serialize");
        let expected = expansion;
        assert_eq!(
            expected, serialized,
            "using {template}, expected: {expected}, got: {serialized}",
        )
    }

    // RFC 6570 section 3.2.1
    #[test]
    fn variable_expansion() {
        test_template("{count}", "one,two,three");
        test_template("{count*}", "one,two,three");
        test_path_template("{/count}", "/one,two,three");
        // test_path_template("{/count*}", "/one/two/three");
        test_template("{;count}", ";count=one,two,three");
        test_template("{;count*}", ";count=one;count=two;count=three");
        test_template("{?count}", "?count=one,two,three");
        test_template("{?count*}", "?count=one&count=two&count=three");
        test_template("{&count*}", "&count=one&count=two&count=three");
    }

    // RFC 6570 section 3.2.2
    #[test]
    fn simple_string_expansion() {
        test_template("{var}", "value");
        test_template("{hello}", "Hello%20World%21");
        test_template("{half}", "50%25");
        test_template("O{empty}X", "OX");
        test_template("O{undef}X", "OX");
        test_template("{x,y}", "1024,768");
        test_template("{x,hello,y}", "1024,Hello%20World%21,768");
        test_template("?{x,empty}", "?1024,");
        test_template("?{x,undef}", "?1024");
        test_template("?{undef,y}", "?768");
        test_template("{var:3}", "val");
        test_template("{var:30}", "value");
        test_template("{list}", "red,green,blue");
        test_template("{list*}", "red,green,blue");
        test_template("{keys}", "semi,%3B,dot,.,comma,%2C");
        test_template("{keys*}", "semi=%3B,dot=.,comma=%2C");
    }

    // RFC 6570 section 3.2.3
    #[test]
    fn reserved_string_expansion() {
        test_template("{+var}", "value");
        test_template("{+hello}", "Hello%20World!");
        test_template("{+half}", "50%25");

        test_template("{base}index", "http%3A%2F%2Fexample.com%2Fhome%2Findex");
        test_template("{+base}index", "http://example.com/home/index");
        test_template("O{+empty}X", "OX");
        test_template("O{+undef}X", "OX");

        // parse XXX test_template("{+path}/here", "/foo/bar/here");
        // parse XXX test_template("/here?ref={+path}", "/here?ref=/foo/bar");
        // parse XXX test_template("up{+path}{var}/here", "up/foo/barvalue/here");
        test_template("{+x,hello,y}", "1024,Hello%20World!,768");
        test_template("{+path,x}/here", "/foo/bar,1024/here");

        test_template("{+path:6}/here", "/foo/b/here");
        test_template("{+list}", "red,green,blue");
        test_template("{+list*}", "red,green,blue");
        test_template("{+keys}", "semi,;,dot,.,comma,,");
        test_template("{+keys*}", "semi=;,dot=.,comma=,");
    }

    // RFC 6570 section 3.2.4
    #[test]
    fn fragment_expansion() {
        test_template("{#var}", "#value");
        test_template("{#hello}", "#Hello%20World!");
        test_template("{#half}", "#50%25");
        test_template("foo{#empty}", "foo#");
        test_template("foo{#undef}", "foo");
        test_template("{#x,hello,y}", "#1024,Hello%20World!,768");
        // parse XXX test_template("{#path,x}/here", "#/foo/bar,1024/here");
        // parse XXX test_template("{#path:6}/here", "#/foo/b/here");
        test_template("{#list}", "#red,green,blue");
        test_template("{#list*}", "#red,green,blue");
        test_template("{#keys}", "#semi,;,dot,.,comma,,");
        test_template("{#keys*}", "#semi=;,dot=.,comma=,");
    }

    // RFC 6570 section 3.2.5
    #[test]
    fn label_expansion() {
        test_template("{.who}", ".fred");
        test_template("{.who,who}", ".fred.fred");
        test_template("{.half,who}", ".50%25.fred");
        test_template("www{.dom*}", "www.example.com");
        test_template("X{.var}", "X.value");
        test_template("X{.empty}", "X.");
        test_template("X{.undef}", "X");
        test_template("X{.var:3}", "X.val");
        test_template("X{.list}", "X.red,green,blue");
        test_template("X{.list*}", "X.red.green.blue");
        test_template("X{.keys}", "X.semi,%3B,dot,.,comma,%2C");
        test_template("X{.keys*}", "X.semi=%3B.dot=..comma=%2C");
        test_template("X{.empty_keys}", "X");
        test_template("X{.empty_keys*}", "X");
    }

    // RFC 6570 section 3.2.6
    #[test]
    fn path_segment_expansion() {
        test_path_template("/who{/who}", "/who/fred");
        test_path_template("{/who,who}", "/fred/fred");
        test_path_template("{/half,who}", "/50%25/fred");
        test_path_template("{/who,dub}", "/fred/me%2Ftoo");
        test_path_template("{/var}", "/value");
        test_path_template("{/var,empty}", "/value/");
        test_path_template("{/var,undef}", "/value");
        test_path_template("{/var,x}/here", "/value/1024/here");
        test_path_template("{/var:1,var}", "/v/value");
        test_path_template("{/list}", "/red,green,blue");
        // XXX should parse test_path_template("{/list*}", "/red/green/blue");
        test_path_template("/pre{/list*}", "/pre/red/green/blue");
        // XXX should parse test_path_template("{/list*,path:4}", "/red/green/blue");
        test_path_template("/pre{/list*,path:4}", "/pre/red/green/blue/%2Ffoo");
        test_path_template("{/keys}", "/semi,%3B,dot,.,comma,%2C");
        // XXX should parse test_path_template("{/keys*}", "/semi=%3B/dot=./comma=%2C");
        test_path_template("/pre{/keys*}", "/pre/semi=%3B/dot=./comma=%2C");
    }

    // RFC 6570 section 3.2.7
    #[test]
    fn path_style_parameter_expansion() {
        test_template("{;who}", ";who=fred");
        test_template("{;half}", ";half=50%25");
        test_template("{;empty}", ";empty");
        test_template("{;v,empty,who}", ";v=6;empty;who=fred");
        test_template("{;v,bar,who}", ";v=6;who=fred");
        test_template("{;x,y}", ";x=1024;y=768");
        test_template("{;x,y,empty}", ";x=1024;y=768;empty");
        test_template("{;x,y,undef}", ";x=1024;y=768");
        test_template("{;hello:5}", ";hello=Hello");
        test_template("{;list}", ";list=red,green,blue");
        test_template("{;list*}", ";list=red;list=green;list=blue");
        test_template("{;keys}", ";keys=semi,%3B,dot,.,comma,%2C");
        test_template("{;keys*}", ";semi=%3B;dot=.;comma=%2C");
    }

    // RFC 6570 section 3.2.8
    #[test]
    fn form_style_query_expansion() {
        test_template("{?who}", "?who=fred");
        test_template("{?half}", "?half=50%25");
        test_template("{?x,y}", "?x=1024&y=768");
        test_template("{?x,y,empty}", "?x=1024&y=768&empty=");
        test_template("{?x,y,undef}", "?x=1024&y=768");
        test_template("{?var:3}", "?var=val");
        test_template("{?list}", "?list=red,green,blue");
        test_template("{?list*}", "?list=red&list=green&list=blue");
        test_template("{?keys}", "?keys=semi,%3B,dot,.,comma,%2C");
        test_template("{?keys*}", "?semi=%3B&dot=.&comma=%2C");
    }

    // RFC 6570 section 3.2.9
    #[test]
    fn form_style_query_continuation() {
        test_template("{&who}", "&who=fred");
        test_template("{&half}", "&half=50%25");
        test_template("?fixed=yes{&x}", "?fixed=yes&x=1024");
        test_template("{&x,y,empty}", "&x=1024&y=768&empty=");
        test_template("{&x,y,undef}", "&x=1024&y=768");

        test_template("{&var:3}", "&var=val");
        test_template("{&list}", "&list=red,green,blue");
        test_template("{&list*}", "&list=red&list=green&list=blue");
        test_template("{&keys}", "&keys=semi,%3B,dot,.,comma,%2C");
        test_template("{&keys*}", "&semi=%3B&dot=.&comma=%2C");
    }
}
