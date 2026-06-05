#![doc = include_str!("../README.md")]

#[warn(missing_docs)]
use std::{collections::HashSet, fmt::Debug, hash::Hash};

use axum::{http, response::IntoResponse};
use hyper::Uri;
use iri_string::{
    spec::IriSpec,
    template::{DynamicContext, UriTemplateStr, UriTemplateString},
    types::IriReferenceString,
};
use regex::Regex;
use render::fill_parts;
use serde::de::DeserializeOwned;
use tracing::{debug, trace};

use crate::parser::VarMod;

use self::{
    parser::{Parsed, Part},
    render::{auth_re_string, axum7_rest, axum7_vars, original_string, path_re_string},
};

pub mod cached;
mod de;
mod parser;
mod render;
mod ser;
// XXX pub use de::UriDeserializationError;
// XXX pub use ser::Error as UriSerializationError;

pub use iri_string::template::context;

/// This is the core trait of the crate - there are two implementators,
/// and by using a trait we ensure that they're interchangable
pub trait Serde6570 {
    /// Applies a [Serialize](serde::Serialize) to produce a URI
    fn expand(
        &self,
        policy: FillPolicy,
        context: impl serde::Serialize,
    ) -> Result<IriReferenceString, Error>;

    /// Parses a Uri into a [Deserialize](serde::Deserialize)
    fn contract<T: DeserializeOwned>(&self, url: Uri) -> Result<T, Error>;

    /// A convenience method for working with Axum: you can use this to produce compatible matchit_routes
    /// in order to have Axum route to a handler, and then use Serde to extract parameters from the
    /// URI
    fn matchit_route(&self) -> String;

    /// Produces a regular expression that matches URIs that we'd parse; in other words,
    /// the regular expression matches URIs that we'd produce with expand
    fn regex(&self) -> Result<Regex, Error>;

    /// Prefixed covers the case where a group of URIs is incorporated into a larger group with a
    /// common path prefix - sometimes referred to as "mounting" a sub-application.
    /// ```rust
    /// # #[derive(Serialize)]
    /// # struct UserData {
    /// #     user: String,
    /// # }
    /// let rt = process(ResourceMappingString("https://example.com/user/{user}", vec![]))?;
    ///
    /// let prefixed = rt.prefixed("/awesome");
    ///
    /// let user_data = UserData {
    ///     user: "nyarly",
    /// };
    /// let expanded = rt.expand(FillPolicy::Relaxed, user_data)?;
    /// assert_eq!(expanded.to_string(), "https://example.com/awesome/user/nyarly");
    /// ```
    fn prefixed(&self, prefix: &str) -> Self;

    /// An option for expanding URLs with non-Serialize data. In general, prefer [expand](Serde6570::expand)
    fn fill(&self, vars: impl Context + Listable) -> Result<IriReferenceString, Error>;

    /// Fills some of the parameters of a template and produce a new template with the
    /// remaining fields available. Useful for producing URIs of "subfamilies" - for example  all
    /// the posts of a particular user
    fn partial_fill(&self, vars: impl Context + Listable + Clone) -> Result<impl Serde6570, Error>;

    /// Produces a UriTempateString for which see uri_template
    fn template(&self) -> Result<UriTemplateString, Error>;

    /// Returns true if there are no template variables in the template - in other words if the
    /// process of filling variables is complete and what remains is effectively a URI.
    fn is_closed(&self) -> bool;
}

/// The general entry point for routing. Pass a ResourceMapping in to get its cached parse.
/// Certain performance oriented applications may want to consider [cached::process]
pub fn process<RM: ResourceMapping + 'static>(rm: RM) -> Result<impl Serde6570, Error> {
    Ok(InnerSingle {
        parsed: parsed(rm)?,
    })
}

#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    #[error("error processing IRI template: {0:?}")]
    IriTempate(#[from] iri_string::template::Error),
    #[error("creating a string for an IRI: {0:?}")]
    CreateString(#[from] iri_string::types::CreationError<std::string::String>),
    #[error("regex parse: {0:?}")]
    RegexParse(#[from] regex::Error),
    #[error("trouble parsing: {0:?}")]
    Parsing(String),
    #[error("missing captures: {0:?}")]
    MissingCaptures(Vec<String>),
    #[error("extra captures: {0:?}")]
    ExtraCaptures(Vec<String>),
    #[error("parse annotation error: {0}")]
    ParseAnnotation(#[from] parser::Error),
    #[error("filling URI template")]
    URITemplating(#[from] ser::Error),
    #[error("for variable {0:?}: two different values: {1:?} vs {2:?}")]
    MismatchedValues(String, String, String),
    #[error("no match: {0:?}")]
    NoMatch(String),
    #[error("unexpected variable names {0:?}")]
    UnexpectedVariables(Vec<String>),
    #[error("capture deserialization: {0:?}")]
    Deserialization(#[from] de::UriDeserializationError),
}

// XXX required here for coherence?
impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        use http::status::StatusCode;
        match self {
            Error::UnexpectedVariables(_)
            | Error::Deserialization(_)
            | Error::MismatchedValues(_, _, _) => {
                (StatusCode::BAD_REQUEST, self.to_string()).into_response()
            }
            Error::IriTempate(_)
            | Error::RegexParse(_)
            | Error::CreateString(_)
            | Error::Parsing(_)
            | Error::MissingCaptures(_)
            | Error::ExtraCaptures(_)
            | Error::URITemplating(_)
            | Error::NoMatch(_)
            | Error::ParseAnnotation(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
            }
        }
    }
}

/// The input for [process] (and [cached::process])
pub trait ResourceMapping: Debug + Clone + Hash + Send + Sync + Eq
where
    Self: 'static,
{
    /// Simply returns the string that should be parsed into a Serde6570
    /// It has to conform to the RFC 6570 grammar, with additional restrictions
    /// that amount to "it should render into a valid URI"
    fn route_template(&self) -> String;

    /// For level 4 of RFC 6570, we need to be able to specify that certain fields
    /// are associated lists in order to sensibly parse them. You can generally leave this empty,
    /// and only return such a list if you know what fields should be treated specially.
    fn assoc_fields(&self) -> Vec<String> {
        vec![]
    }
}

/// A convenient implementation of [ResourceMapping] - the two fields are the route_template and
/// assoc_fields
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ResourceMappingString(pub String, pub Vec<String>);

impl ResourceMapping for ResourceMappingString {
    fn route_template(&self) -> String {
        self.0.clone()
    }

    fn assoc_fields(&self) -> Vec<String> {
        self.1.clone()
    }
}

fn parsed<RT: ResourceMapping>(rt: RT) -> Result<Parsed, Error> {
    let mut parsed =
        parser::parse(&rt.route_template()).map_err(|e| Error::Parsing(format!("{:?}", e)))?;
    parsed.annotate_assocs(rt.assoc_fields())?;
    trace!("Parsed {rt:?} into {parsed:?}");
    Ok(parsed)
}

#[derive(Clone, Copy, Debug)]
pub enum FillPolicy {
    Relaxed,
    NoMissing,
    NoExtra,
    Strict,
    DropMissing,
}

use iri_string::template::context::{Context, Visitor};

pub trait Listable {
    fn list_vars(&self) -> Vec<String>;
}

#[derive(Clone)]
pub struct VarsList<L: IntoIterator<Item = (String, String)>>(pub L);

impl<L: Clone + IntoIterator<Item = (String, String)>> Listable for VarsList<L> {
    fn list_vars(&self) -> Vec<String> {
        self.0.clone().into_iter().map(|(k, _)| k.clone()).collect()
    }
}

impl<L: Clone + IntoIterator<Item = (String, String)>> Context for VarsList<L> {
    fn visit<V: Visitor>(&self, visitor: V) -> V::Result {
        let visited = visitor.var_name().as_str();
        // barf - complexity here is awful
        match self.0.clone().into_iter().find(|(k, _)| k == visited) {
            Some((_, v)) => visitor.visit_string(v),
            None => visitor.visit_undefined(),
        }
    }
}

pub(crate) struct PolicyContext<C: Context + Listable> {
    provided: HashSet<String>,
    extra: HashSet<String>,
    missing: HashSet<String>,
    inner: C,
}

impl<C: Context + Listable> PolicyContext<C> {
    fn new(inner: C) -> Self {
        Self {
            provided: HashSet::new(),
            extra: HashSet::new(),
            missing: HashSet::new(),
            inner,
        }
    }

    fn check(&self, policy: FillPolicy) -> Result<(), Error> {
        match (policy, self.missing.is_empty(), self.extra.is_empty()) {
            (FillPolicy::Strict, false, _) | (FillPolicy::NoMissing, false, _) => Err(
                Error::MissingCaptures(self.missing.iter().cloned().collect()),
            ),
            (FillPolicy::Strict, _, false) | (FillPolicy::NoExtra, _, false) => {
                Err(Error::ExtraCaptures(self.extra.iter().cloned().collect()))
            }
            _ => Ok(()),
        }
    }
}

impl<C: Context + Listable> DynamicContext for PolicyContext<C> {
    fn on_expansion_start(&mut self) {
        trace!("on_expansion_start");
        self.provided.clear();
        self.extra.clear();
        self.missing.clear();
        for v in self.inner.list_vars() {
            self.provided.insert(v.clone());
            self.extra.insert(v);
        }
        trace!(
            "on_expansion_start: provided: {:?} extra: {:?} missing {:?}",
            self.provided, self.extra, self.missing
        );
    }

    fn visit_dynamic<V: Visitor>(&mut self, visitor: V) -> V::Result {
        let k = visitor.var_name().as_str();
        trace!("URI template fill: {:?}", k);
        self.extra.remove(k);
        if !self.provided.contains(k) {
            self.missing.insert(k.to_string());
        }
        trace!(
            "URI template fill: provided: {:?} extra: {:?} missing {:?}",
            self.provided, self.extra, self.missing
        );
        self.inner.visit(visitor)
    }
}
#[derive(Default, Clone)]
struct InnerSingle {
    parsed: Parsed,
}

impl Serde6570 for InnerSingle {
    fn matchit_route(&self) -> String {
        let mut out = "".to_string();

        for part in &self.parsed.path {
            match part {
                Part::Lit(l) => out.push_str(l),
                Part::Expression(exp) | Part::SegPathVar(exp) => {
                    out.push_str(&axum7_vars(&exp.varspecs))
                }
                Part::SegRest(exp) | Part::SegPathRest(exp) => {
                    out.push_str(&axum7_rest(&exp.varspecs))
                }
            }
        }

        out
    }

    fn regex(&self) -> Result<Regex, Error> {
        Regex::new(&self.re_str()).map_err(|e| e.into())
    }

    fn prefixed(&self, prefix: &str) -> Self {
        let mut prefixed = InnerSingle {
            parsed: self.parsed.clone(),
        };
        prefixed.parsed.path.insert(0, Part::Lit(prefix.to_owned()));
        prefixed
    }

    fn expand(
        &self,
        policy: FillPolicy,
        context: impl serde::Serialize,
    ) -> Result<IriReferenceString, Error> {
        Ok(ser::fill(&self.parsed, policy, context)?.try_into()?)
    }

    fn contract<T: DeserializeOwned>(&self, uri: Uri) -> Result<T, Error> {
        let parsed = &self.parsed;
        let regex = self.regex()?;

        let de = de::UriDeserializer::for_uri(&uri, parsed, &regex)?;

        T::deserialize(de).map_err(Error::from)
    }

    fn fill(&self, vars: impl Context + Listable) -> Result<IriReferenceString, Error> {
        self.fill_uritemplate(FillPolicy::NoMissing, vars)
    }

    fn partial_fill(&self, vars: impl Context + Listable + Clone) -> Result<impl Serde6570, Error> {
        self.partial_fill_single(vars)
    }

    fn template(&self) -> Result<UriTemplateString, Error> {
        let string = self.template_string();
        let t = UriTemplateStr::new(&string)?;
        Ok(t.into())
    }

    fn is_closed(&self) -> bool {
        self.expressions().is_empty()
    }
}

impl InnerSingle {
    fn expressions(&self) -> Vec<Part> {
        self.parsed
            .parts_iter()
            .filter(|part| !matches!(part, Part::Lit(_)))
            .cloned()
            .collect()
    }

    fn re_str(&self) -> String {
        let re = self
            .parsed
            .auth
            .iter()
            .flatten()
            .map(auth_re_string)
            .chain(self.parsed.path.iter().map(path_re_string))
            .collect::<Vec<_>>()
            .join("");
        re
    }

    fn template_string(&self) -> String {
        self.parsed
            .parts_iter()
            .map(original_string)
            .collect::<Vec<_>>()
            .join("")
    }

    fn partial_fill_single(&self, vars: impl Context + Listable + Clone) -> Result<Self, Error> {
        let mut prefixed = InnerSingle {
            parsed: self.parsed.clone(),
        };
        prefixed
            .parsed
            .auth
            .as_ref()
            .map(|a| fill_parts(&a, &vars))
            .transpose()?;

        prefixed.parsed.path = fill_parts(&self.parsed.path, &vars)?;

        prefixed.parsed.query = self
            .parsed
            .query
            .as_ref()
            .map(|q| fill_parts(&q, &vars))
            .transpose()?;

        Ok(prefixed)
    }

    fn fill_uritemplate(
        &self,
        policy: FillPolicy,
        vars: impl Context + Listable,
    ) -> Result<IriReferenceString, Error> {
        let mut pol = PolicyContext::new(vars);

        let templ = &self.template()?;
        let expanded = templ.expand_dynamic_to_string::<IriSpec, _>(&mut pol)?;
        debug!("expanded {}", expanded);
        pol.check(policy)?;
        debug!("checked {:?}", expanded);
        Ok(expanded
            .try_into()
            .inspect_err(|e| debug!("try_into: {e:?}"))?)
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use tracing_test::traced_test;

    use super::*;

    fn quick_route(input: &str) -> InnerSingle {
        InnerSingle {
            parsed: parser::parse(input).unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn round_trip() {
        let input = "http://example.com/user/{user_id}{?something,mysterious}";

        let route = quick_route(input);

        assert_eq!(route.template_string(), input.to_string())
    }

    #[test]
    fn prefixing() {
        let route = quick_route("http://example.com/user/{user_id}{?something,mysterious}");
        let prefixed = route.prefixed("/api");
        assert_eq!(
            prefixed.template_string(),
            "http://example.com/api/user/{user_id}{?something,mysterious}".to_string() //                  ^^^^
        )
    }

    #[test]
    #[traced_test]
    fn partial_fill() {
        let route = quick_route("http://example.com{/something,mysterious}/user{/user_id}");
        let mut vars = HashMap::new();
        vars.insert("something".to_string(), "S".to_string());
        vars.insert("mysterious".to_string(), "M".to_string());
        let tmpl_r = route.partial_fill(VarsList(vars)).unwrap().template();
        let tmpl = tmpl_r.unwrap();
        assert_eq!(
            tmpl.to_string(),
            "http://example.com/S/M/user{/user_id}".to_string()
        )
    }

    #[test]
    fn axum_routes() {
        let rc = quick_route("/api/event/{event_id}");
        assert_eq!(rc.matchit_route(), "/api/event/:event_id".to_string());
    }
    /*
     * Considerations for regexp:
     * if lits include //, we can sensibly match variables in auth part;
     * otherwise, we have to match '[^/]* //[^/]*' for the authority
     */
    #[test]
    fn regex() {
        let route = quick_route(
            "http://{domain}/user/{user_id}/file{/file_id}?something={good}{&mysterious}",
        );
        assert_eq!(
            route.re_str(),
            "http://(?<domain>[^/,]*)/user/(?<user_id>[^/?#,]*)/file/(?<file_id>[^/?#/]*)"
        );
    }

    /*
    * XXX removed .extract
    #[test]
    fn extraction() {
        let route = quick_route("http://{domain}/user/{user_id}/file{/file_id}?something={good}{&mysterious}");
        let uri = "http://example.com/user/me@nowhere.org/file/17?something=weird&mysterious=100";
        assert_eq!(
            route.extract::<(String, String, u16)>(uri).unwrap(),
            ("example.com".to_string(), "me@nowhere.org".to_string(), 17)
        );
    }

    #[test]
    fn extraction_errors() {
        let route = quick_route("http://{domain}/user/{user_id}/file{/file_id}?something={good}{&mysterious}");
        let uri = "http://example.com/user/me@nowhere.org/file?something=weird&mysterious=100";
        assert!(matches!(
            route.extract::<(String, String, u16)>(uri),
            Err(Error::NoMatch(_,_))
        ));
    }
    */
}
