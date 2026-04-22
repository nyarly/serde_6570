use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    hash::Hash,
    sync::{Arc, Mutex, OnceLock, RwLock},
};

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
use mattak_hypermedia::{Affordance, Operation};

use self::{
    parser::{Parsed, Part},
    render::{auth_re_string, axum7_rest, axum7_vars, original_string, path_re_string},
};

use typemap_ors::ShareMap;

mod de;
pub mod extract;
mod parser;
mod render;
mod ser;
// XXX pub use de::UriDeserializationError;
// XXX pub use ser::Error as UriSerializationError;

pub use iri_string::template::context;

#[derive(thiserror::Error, Debug)]
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
    #[error("hypermedia error: {0:?}")]
    Hypermedia(#[from] mattak_hypermedia::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        use http::status::StatusCode;
        match self {
            Error::Hypermedia(e) => e.into_response(),
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
/// This is used by the Route derive macro to validate that templates align with fields
/// Not advised for general use - the parse of the template might panic, and isn't cached anywhere.
/// Instead, see route_config
pub fn template_vars(template: &str) -> Vec<String> {
    let parsed = parser::parse(template).unwrap();

    parsed
        .parts_iter()
        .filter_map(|part| match part {
            Part::Lit(_) => None,
            Part::Expression(expression)
            | Part::SegVar(expression)
            | Part::SegPathVar(expression)
            | Part::SegRest(expression)
            | Part::SegPathRest(expression) => Some(
                expression
                    .varspecs
                    .iter()
                    .map(|vs| vs.varname.clone())
                    .collect::<Vec<_>>(),
            ),
        })
        .flatten()
        .collect::<Vec<_>>()
}

pub trait Route {
    fn route_template() -> RouteTemplateString;

    fn axum_route() -> String {
        route_config(Self::route_template()).axum_route()
    }
}

pub trait RouteTemplate: Debug + Clone + Hash + Send + Sync + Eq
where
    Self: 'static,
{
    // XXX really nice to have this as an associated method...
    // Would like this one structs and have it work with Self,
    // and maybe with Enum instances.
    fn route_template(&self) -> String;

    fn assoc_fields(&self) -> Vec<String> {
        vec![]
    }

    // XXX have to be able to hash the template,
    // _but_ it could hash the String
    // _but, OTOH_ it would be better to avoid
    //   instantiating a string for a parsed template
    fn prefixed(self, at: &str) -> Entry {
        route_config(self).prefixed(at)
    }
}

struct RTKey<RT: RouteTemplate>(RT);

impl<RT: RouteTemplate + 'static> typemap_ors::Key for RTKey<RT> {
    type Value = Arc<Mutex<Map<RT>>>;
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct RouteTemplateString(pub String, pub Vec<String>);

impl RouteTemplate for RouteTemplateString {
    fn route_template(&self) -> String {
        self.0.clone()
    }

    fn assoc_fields(&self) -> Vec<String> {
        self.1.clone()
    }
}

pub(crate) struct Map<RT: RouteTemplate> {
    templates: HashMap<RT, String>,
    store: HashMap<String, Arc<RwLock<InnerSingle>>>,
}

impl<RT: RouteTemplate> Default for Map<RT> {
    fn default() -> Self {
        Self {
            templates: Default::default(),
            store: Default::default(),
        }
    }
}

// static THE_MAP:  OnceLock<Arc<Mutex<Map>>> = OnceLock::new();
static THE_MAP: OnceLock<Arc<Mutex<ShareMap>>> = OnceLock::new();

fn the_map<RT: RouteTemplate + 'static>() -> Arc<Mutex<Map<RT>>> {
    let arcmutex = THE_MAP
        .get_or_init(|| Arc::new(Mutex::new(ShareMap::custom())))
        .clone();
    let mut typed = arcmutex.lock().expect("type map not to be poisoned");
    typed
        .entry::<RTKey<RT>>()
        .or_insert_with(|| Arc::new(Mutex::new(Map::<RT>::default())))
        .clone()
}

fn parsed<RT: RouteTemplate>(rt: RT) -> Result<Parsed, Error> {
    let mut parsed =
        parser::parse(&rt.route_template()).map_err(|e| Error::Parsing(format!("{:?}", e)))?;
    parsed.annotate_assocs(rt.assoc_fields())?;
    trace!("Parsed {rt:?} into {parsed:?}");
    Ok(parsed)
}

impl<RT: RouteTemplate> Map<RT> {
    fn named(&mut self, rt: RT) -> Result<Arc<RwLock<InnerSingle>>, Error> {
        let template = self
            .templates
            .entry(rt.clone())
            .or_insert_with(|| format!("{};{:?}", rt.route_template(), rt.assoc_fields()));
        if self.store.contains_key(template) {
            self.store
                .get(template)
                .ok_or(Error::Parsing(
                    "couldn't get value for contained key".to_string(),
                ))
                .cloned()
        } else {
            let route = Arc::new(RwLock::new(InnerSingle {
                parsed: parsed(rt)?,
                ..InnerSingle::default()
            }));
            self.store.insert(template.to_string(), route);
            self.store
                .get(template)
                .ok_or(Error::Parsing(
                    "couldn't get value for just-inserted key".to_string(),
                ))
                .cloned()
        }
    }
}

/// The general entry point for routing. Pass a RouteTemplate in to get its cached parse,
/// as an Entry. From there you can call methods to template URIs, match strings etc etc.
///
/// # Panics
///
/// Panics if the routing cache becomes poisoned, which doesn't happen by design.
/// A panic on this function constitutes a bug.
pub fn route_config(rm: impl RouteTemplate + 'static) -> Entry {
    let arcmutex = the_map();
    let mut map = arcmutex.lock().expect("route map not to be poisoned");
    let inner = map.named(rm).expect("routes to be parseable");
    Entry {
        inner: inner.clone(),
    }
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
            self.provided,
            self.extra,
            self.missing
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
            self.provided,
            self.extra,
            self.missing
        );
        self.inner.visit(visitor)
    }
}

// We have a RwLock here because we would like to be able to cache rendering in the InnerSingle To
// do that, we'd need to be able to accept a &mut self, or else replace the innersingle with a
// cloned version where we update the values (imagine InnerSingle with OnceLocks for many of its
// methods) at some point, we might also decide that a given InnerSingle is close enough to done
// and finish its rendering, and have a FixedSingle or something. Or just start there: render out
// all the things a given route might need and cache that. For the time being, we'll render each
// time (and just get read locks), but at some point in the future there's another round of
// over-engineering to tackle
pub struct Entry {
    inner: Arc<RwLock<InnerSingle>>,
}

impl Entry {
    pub fn axum_route(&self) -> String {
        let inner = self.inner.read().expect("not poisoned");
        inner.axum_route()
    }

    pub fn serialize(
        &self,
        policy: FillPolicy,
        context: impl serde::Serialize,
    ) -> Result<IriReferenceString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.serialize(policy, context)
    }

    pub fn fill(&self, vars: impl Context + Listable) -> Result<IriReferenceString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.fill_uritemplate(FillPolicy::NoMissing, vars)
    }

    pub fn partial_fill(
        &self,
        vars: impl IntoIterator<Item = (String, String)> + Clone,
    ) -> Result<UriTemplateString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.partial_fill(VarsList(vars))
    }

    pub fn template(&self) -> Result<UriTemplateString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.template()
    }

    pub fn affordance(&self, name: String, ops: Vec<Operation>) -> Result<Affordance, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.affordance(name, ops)
    }

    pub fn hydra_type(&self) -> String {
        let inner = self.inner.read().expect("not poisoned");
        inner.hydra_type()
    }

    /*
    pub fn extract<T: DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.extract(url)
    }
    */

    pub fn from_uri<T: DeserializeOwned>(&self, url: Uri) -> Result<T, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.from_uri(url)
    }

    pub fn prefixed(&self, prefix: &str) -> Entry {
        let mut inner = self.inner.write().expect("not poisoned");
        Entry {
            inner: Arc::new(RwLock::new(inner.prefixed(prefix))),
        }
    }
}

#[derive(Default, Clone)]
struct InnerSingle {
    parsed: Parsed,
    prefixes: HashMap<Arc<str>, InnerSingle>,
    regex: OnceLock<Result<Regex, regex::Error>>,
}

impl InnerSingle {
    fn expressions(&self) -> Vec<Part> {
        self.parsed
            .parts_iter()
            .filter(|part| !matches!(part, Part::Lit(_)))
            .cloned()
            .collect()
    }

    fn affordance(&self, name: String, ops: Vec<Operation>) -> Result<Affordance, Error> {
        if self.expressions().is_empty() {
            let empty = iri_string::template::simple_context::SimpleContext::new();
            let id = self.template()?.expand(&empty)?.try_into()?;
            Ok(Affordance::Link { id, operation: ops })
        } else {
            Ok(Affordance::IriTemplate {
                id: name.try_into()?,
                template: self.template()?,
                operation: ops,
            })
        }
    }
    /*
    let entry = |rm: RouteMap, ops| {
        let prefixed = rm.prefixed(nested_at);
        let url_attr = if prefixed.hydra_type() == "Link" {
            "id"
        } else {
            "template"
        };
        let url_template = prefixed.template().expect("a legit URITemplate");
        json!({
            "type": prefixed.hydra_type(),
            url_attr: url_template,
            "operation": ops
        })
    };
    */

    fn hydra_type(&self) -> String {
        if self.expressions().is_empty() {
            "Link".to_string()
        } else {
            "IriTemplate".to_string()
        }
    }

    fn prefixed(&mut self, prefix: &str) -> Self {
        let prefix_owned = prefix.into();
        if self.prefixes.contains_key(&prefix_owned) {
            self.prefixes
                .get(&prefix_owned)
                .expect("couldn't get value for contained key")
                .clone()
        } else {
            let mut prefixed = InnerSingle { ..self.clone() };
            prefixed.parsed.path.insert(0, Part::Lit(prefix.to_owned()));
            self.prefixes.insert(prefix_owned, prefixed.clone());
            prefixed
        }
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

    // definitely consider caching this result
    fn regex(&self) -> Result<&Regex, regex::Error> {
        self.regex
            .get_or_init(|| Regex::new(&self.re_str()))
            .as_ref()
            .map_err(|e| e.clone())
    }

    fn from_uri<T: DeserializeOwned>(&self, uri: Uri) -> Result<T, Error> {
        let parsed = &self.parsed;
        let regex = self.regex()?;

        let de = de::UriDeserializer::for_uri(&uri, parsed, regex)?;

        T::deserialize(de).map_err(Error::from)
    }

    fn template_string(&self) -> String {
        self.parsed
            .parts_iter()
            .map(original_string)
            .collect::<Vec<_>>()
            .join("")
    }

    fn template(&self) -> Result<UriTemplateString, Error> {
        let string = self.template_string();
        let t = UriTemplateStr::new(&string)?;
        Ok(t.into())
    }

    fn serialize(
        &self,
        policy: FillPolicy,
        context: impl serde::Serialize,
    ) -> Result<IriReferenceString, Error> {
        Ok(ser::fill(&self.parsed, policy, context)?.try_into()?)
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

    fn partial_fill(
        &self,
        vars: impl Context + Listable + Clone,
    ) -> Result<UriTemplateString, Error> {
        let filled_string = self
            .parsed
            .auth
            .clone()
            .map_or(Ok(vec![]), |a| fill_parts(&a, &vars))?
            .iter()
            .map(original_string)
            .chain(
                fill_parts(&self.parsed.path, &vars)?
                    .iter()
                    .map(original_string),
            )
            .chain(
                self.parsed
                    .query
                    .clone()
                    .map_or(Ok(vec![]), |q| fill_parts(&q, &vars))?
                    .iter()
                    .map(original_string),
            )
            .collect::<Vec<_>>()
            .join("");

        let t = UriTemplateStr::new(&filled_string)?;
        Ok(t.into())
    }

    fn axum_route(&self) -> String {
        let mut out = "".to_string();

        for part in &self.parsed.path {
            match part {
                Part::Lit(l) => out.push_str(l),
                Part::Expression(exp) | Part::SegVar(exp) | Part::SegPathVar(exp) => {
                    out.push_str(&axum7_vars(&exp.varspecs))
                }
                Part::SegRest(exp) | Part::SegPathRest(exp) => {
                    out.push_str(&axum7_rest(&exp.varspecs))
                }
            }
        }

        out
    }
}

#[cfg(test)]
mod test {
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
        let mut route = quick_route("http://example.com/user/{user_id}{?something,mysterious}");
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
        let tmpl_r = route.partial_fill(VarsList(vars));
        debug!("{:?}", tmpl_r);
        let tmpl = tmpl_r.unwrap();
        assert_eq!(
            tmpl.to_string(),
            "http://example.com/S/M/user{/user_id}".to_string()
        )
    }

    #[test]
    fn axum_routes() {
        let rc = quick_route("/api/event/{event_id}");
        assert_eq!(rc.axum_route(), "/api/event/:event_id".to_string());
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
