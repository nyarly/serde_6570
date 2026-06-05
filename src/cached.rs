use crate::Serde6570;
use crate::{Error, FillPolicy, InnerSingle, Listable, ResourceMapping, parsed};

use hyper::Uri;
use iri_string::template::Context;
use iri_string::template::UriTemplateString;
use iri_string::types::IriReferenceString;
use regex::Regex;
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, RwLock},
};
use typemap_ors::ShareMap;

pub(crate) struct Map<RM: ResourceMapping> {
    templates: HashMap<RM, String>,
    store: HashMap<String, Entry>,
}

impl<RM: ResourceMapping> Default for Map<RM> {
    fn default() -> Self {
        Self {
            templates: Default::default(),
            store: Default::default(),
        }
    }
}
impl<RM: ResourceMapping> Map<RM> {
    fn named(&mut self, rt: RM) -> Result<Entry, Error> {
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
            let route = Entry {
                inner: Arc::new(RwLock::new(InnerSingle {
                    parsed: parsed(rt)?,
                })),
                prefixes: Default::default(),
                regex: Default::default(),
            };
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

static THE_MAP: OnceLock<Arc<Mutex<ShareMap>>> = OnceLock::new();
fn the_map<RM: ResourceMapping + 'static>() -> Arc<Mutex<Map<RM>>> {
    let arcmutex = THE_MAP
        .get_or_init(|| Arc::new(Mutex::new(ShareMap::custom())))
        .clone();
    let mut typed = arcmutex.lock().expect("type map not to be poisoned");
    typed
        .entry::<RMKey<RM>>()
        .or_insert_with(|| Arc::new(Mutex::new(Map::<RM>::default())))
        .clone()
}

struct RMKey<RM: ResourceMapping>(RM);

impl<RM: ResourceMapping + 'static> typemap_ors::Key for RMKey<RM> {
    type Value = Arc<Mutex<Map<RM>>>;
}

/// The general entry point for routing. Pass a [ResourceMapping] in to get its cached parse,
/// as an [Entry]. From there you can call methods to template URIs, match strings etc etc.
///
/// # Panics
///
/// Panics if the routing cache becomes poisoned, which doesn't happen by design.
/// A panic on this function constitutes a bug.
pub fn process<RM: ResourceMapping + 'static>(rm: RM) -> Result<impl Serde6570, Error> {
    let arcmutex = the_map();
    let mut map = arcmutex.lock().expect("route map not to be poisoned");
    Ok(map.named(rm)?)
}

// Entry is a convenience wrapper around the Arc<RwLock<InnerSingle>> - mostly, it mediates reaching into the
// smart pointers to manage the method calls.
//
// We have a RwLock here because we would like to be able to cache rendering in the InnerSingle To
// do that, we'd need to be able to accept a &mut self, or else replace the innersingle with a
// cloned version where we update the values (imagine InnerSingle with OnceLocks for many of its
// methods) at some point, we might also decide that a given InnerSingle is close enough to done
// and finish its rendering, and have a FixedSingle or something. Or just start there: render out
// all the things a given route might need and cache that. For the time being, we'll render each
// time (and just get read locks), but at some point in the future there's another round of
// over-engineering to tackle

/// This is the cached implementor for Serde6570; for a long running application,
/// you probably want to prefer this (and the corresponding [process] that produces it)
/// over the default (private) struct. For one off usage, the machinery for safely
/// caching the templates may be too heavyweight.
#[derive(Clone)]
pub struct Entry {
    inner: Arc<RwLock<InnerSingle>>,
    prefixes: Arc<Mutex<HashMap<String, Entry>>>,
    regex: OnceLock<Result<Regex, Error>>,
}

impl Serde6570 for Entry {
    fn matchit_route(&self) -> String {
        let inner = self.inner.read().expect("not poisoned");
        inner.matchit_route()
    }

    fn regex(&self) -> Result<regex::Regex, Error> {
        self.regex
            .get_or_init(|| {
                let inner = self.inner.read().expect("not poisoned");
                inner.regex()
            })
            .clone()
    }

    fn prefixed(&self, prefix: &str) -> Self {
        let mut map = self.prefixes.lock().expect("prefix map not to be poisoned");

        if map.contains_key(prefix) {
            map.get(prefix)
                .expect("couldn't get value for contained key")
                .clone()
        } else {
            let inner = self.inner.read().expect("not poisoned");
            let prefixed = inner.prefixed(prefix);
            let entry = Entry {
                inner: Arc::new(RwLock::new(prefixed)),
                prefixes: Default::default(),
                regex: Default::default(),
            };
            map.insert(prefix.to_string(), entry);
            map.get(prefix)
                .expect("couldn't get value for just-inserted key")
                .clone()
        }
    }

    fn expand(
        &self,
        policy: FillPolicy,
        context: impl serde::Serialize,
    ) -> Result<IriReferenceString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.expand(policy, context)
    }

    fn contract<T: DeserializeOwned>(&self, url: Uri) -> Result<T, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.contract(url)
    }

    fn fill(&self, vars: impl Context + Listable) -> Result<IriReferenceString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.fill_uritemplate(FillPolicy::NoMissing, vars)
    }

    fn partial_fill(&self, vars: impl Context + Listable + Clone) -> Result<impl Serde6570, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.partial_fill_single(vars)
    }

    fn template(&self) -> Result<UriTemplateString, Error> {
        let inner = self.inner.read().expect("not poisoned");
        inner.template()
    }

    fn is_closed(&self) -> bool {
        let inner = self.inner.read().expect("not poisoned");
        inner.is_closed()
    }
}
