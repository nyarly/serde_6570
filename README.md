# Serde 6570

This crate provides a Serde implementation for
a subset of [RFC 6570 URI Templates](https://datatracker.ietf.org/doc/html/rfc6570).
Specifically, the allowed grammar is restricted
to enforce conformance to the RFC 3986 grammar for URIs.

## Implications

What does that mean exactly?

It means that we can write a RFC 6570 template,
and use it to [expand](Serde6570::expand) from any [Serialize](serde::Serialize) data
to a URI.
```rust
use serde::Serialize;
use serde_6570::{process, ResourceMappingString, FillPolicy, Serde6570};

#[derive(Serialize)]
struct PostData {
    user: String,
    post_id: u16,
}

let post_data = PostData {
    user: "nyarly".into(),
    post_id: 187
};

let rt = process(ResourceMappingString("https://example.com/user/{user}/post/{post_id}".into(), vec![])).unwrap();
let expanded = rt.expand(FillPolicy::Relaxed, post_data).unwrap();
assert_eq!(expanded.to_string(), "https://example.com/user/nyarly/post/187".to_string());
```

We can also use the same template to [contract](Serde6570::contract)
from a URI
into a [Deserialize](serde::Deserialize):
```rust
use serde::Deserialize;
use serde_6570::{process, ResourceMappingString, FillPolicy, Serde6570};
#[derive(Deserialize,PartialEq,Debug)]
struct PostData {
    user: String,
    post_id: u16,
}

let rt = process(ResourceMappingString("https://example.com/user/{user}/post/{post_id}".into(), vec![])).unwrap();

let post_data: PostData = rt.contract("https://example.com/user/nyarly/post/187".parse().unwrap()).unwrap();

assert_eq!(post_data, PostData {
    user: "nyarly".into(),
    post_id: 187
});
```

## Motivation

Who would want such a thing, and why?

This primary audience for this crate is web application authors,
in the context where application URLs correspond to families of resources
that are generic over attributes of the individual resources.

If you write code for the web, this probably means you.

Web applications often use URLs to transmit data about their resources,
embedding attributes like user names and resource IDs in the URL.
Many web application frameworks provide application authors with
routing functionality to match their URIs
and to extract those attributes from the URI.
Some provide the utility to render the corresponding URIs for their
internal representations.

This crate is an effort to standardize this dual approach,
at least for Rust.
By using a IETF standards, we hope to provide tooling that covers
not only the case of a web application rendering and routing
its own URIs,
but also rendering and interpreting the URIs of foreign web applications
with some facility.

Furthermore, by using Serde, we can parse URIs directly into well known types,
and control for whole categories of errors related to
handling data from outside our system.

## Usage

You'll want to focus primarily on [process] and the [Serde6570] trait.
