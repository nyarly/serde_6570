use ::core::{future::Future, pin::Pin};
use std::sync::Arc;

use axum::{
    extract::{rejection::NestedPathRejection, FromRef, FromRequestParts, NestedPath},
    http::request::Parts,
    response::IntoResponse,
};
use hyper::Uri;
use serde::{de::DeserializeOwned, Serialize};

use mattak_hypermedia::{Affordance, Operation, ResourceFields};

use crate::{route_config, Entry, Error, FillPolicy, Route, RouteTemplate as _};

pub trait ExtractedRoute {
    type Nick: Route + Clone;
    fn entry(&self) -> Entry;
    fn nick(&self) -> Self::Nick;

    fn resource_fields(
        &self,
        api_name: &str,
        operation: Vec<Operation>,
    ) -> Result<ResourceFields<Self::Nick>, Error>
    where
        <Self as ExtractedRoute>::Nick: Serialize,
    {
        let entry = self.entry();
        let id = entry.serialize(FillPolicy::Strict, self.nick())?;
        let template = entry.template()?;
        ResourceFields::new(id, template, self.nick(), api_name, operation).map_err(Error::from)
    }

    fn affordance(&self, name: impl ToString, ops: Vec<Operation>) -> Affordance {
        self.entry()
            .affordance(name.to_string(), ops)
            .expect("entries to produce affordances")
    }
}

pub struct NestedRoute<R> {
    pub nested_path: Arc<str>,
    pub nick: R,
}

impl<R> NestedRoute<R> {
    pub fn default_relative_route<N: Default>(&self, rel: impl ToString) -> NestedRoute<N> {
        let nested_path = nest_rel(self.nested_path.clone(), rel.to_string());
        NestedRoute {
            nested_path,
            nick: N::default(),
        }
    }

    pub fn relative_route<N>(&self, rel: impl ToString, nick: N) -> NestedRoute<N> {
        let nested_path = nest_rel(self.nested_path.clone(), rel.to_string());
        NestedRoute { nested_path, nick }
    }
}

impl<R: Route + Clone> ExtractedRoute for NestedRoute<R> {
    type Nick = R;

    fn entry(&self) -> Entry {
        R::route_template().prefixed(self.nested_path.as_ref())
    }

    fn nick(&self) -> Self::Nick {
        self.nick.clone()
    }
}

// XXX tests
fn nest_rel(nested: Arc<str>, rel: String) -> Arc<str> {
    if rel == "" || rel == "." {
        return nested.clone();
    }

    let mut parts: Vec<_> = nested.as_ref().split("/").collect();
    for relpart in rel.split("/") {
        match relpart {
            ".." => {
                parts.pop();
            }
            "." | "" => (),
            seg => parts.push(seg),
        }
    }
    parts.join("/").into()
}

impl<R, S> FromRequestParts<S> for NestedRoute<R>
where
    R: Route + DeserializeOwned,
    S: Send + Sync,
{
    /// If the extractor fails it'll use this "rejection" type. A rejection is
    /// a kind of error that can be converted into a response.
    type Rejection = Rejection;

    ///  Perform the extraction.
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut Parts,
        state: &'life1 S,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Self, Self::Rejection>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(extract_nested(parts, state))
    }
}

pub async fn extract_nested<R, S>(parts: &mut Parts, state: &S) -> Result<NestedRoute<R>, Rejection>
where
    R: Route + DeserializeOwned,
    S: Send + Sync,
{
    let nested_path = NestedPath::from_request_parts(parts, state)
        .await?
        .as_str()
        .into();

    let rt = R::route_template();
    let cfg = route_config(rt);
    let route = cfg.from_uri(parts.uri.clone())?;

    Ok(NestedRoute {
        nested_path,
        nick: route,
    })
}

pub struct CanonicalUri(Uri);

pub struct CanonRoute<R: Route> {
    pub uri: Uri,
    pub nested_path: Arc<str>,
    pub nick: R,
}

impl<R: Route + Clone> ExtractedRoute for CanonRoute<R> {
    type Nick = R;

    fn entry(&self) -> Entry {
        R::route_template().prefixed(self.nested_path.as_ref())
    }

    fn nick(&self) -> Self::Nick {
        self.nick.clone()
    }
}

impl<R, S> FromRequestParts<S> for CanonRoute<R>
where
    R: Route + DeserializeOwned,
    CanonicalUri: FromRef<S>,
    S: Send + Sync,
{
    /// If the extractor fails it'll use this "rejection" type. A rejection is
    /// a kind of error that can be converted into a response.
    type Rejection = Rejection;

    /// Perform the extraction.
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut Parts,
        state: &'life1 S,
    ) -> ::core::pin::Pin<
        Box<
            dyn ::core::future::Future<Output = Result<Self, Self::Rejection>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(extract_canon(parts, state))
    }
}

pub async fn extract_canon<R, S>(parts: &mut Parts, state: &S) -> Result<CanonRoute<R>, Rejection>
where
    R: Route + DeserializeOwned,
    S: Send + Sync,
    CanonicalUri: FromRef<S>,
{
    let uri = CanonicalUri::from_ref(&state).0;

    let nested_path = NestedPath::from_request_parts(parts, state)
        .await?
        .as_str()
        .into();

    let rt = R::route_template();
    let cfg = route_config(rt);
    let route = cfg.from_uri(parts.uri.clone())?;

    Ok(CanonRoute {
        uri,
        nested_path,
        nick: route,
    })
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Rejection {
    #[error("couldn't get nested path {0:?}")]
    NestedPath(#[from] NestedPathRejection),
    #[error("top-level error {0:?}")]
    TopLevel(#[from] Error), // XXX barf
}

impl IntoResponse for Rejection {
    fn into_response(self) -> axum::response::Response {
        match self {
            Rejection::NestedPath(npr) => npr.into_response(),
            Rejection::TopLevel(error) => error.into_response(),
        }
    }
}
