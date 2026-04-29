# Serde 6570

Ruby on Rails has a powerful feature: bidirectional routing.

A single configuration does two things: both
describes routing incoming requests to the Controller class, and extracts parameters from the URL.
and provides templates for URLs based on those parameters.
This mains that URL structure is outside of the Controller interface.

Sadly, Rails routing (is hardly alone) in inventing a bespoke little language for its routing.
Likewise matchit (and therefore e.g. Axum) have a little language
for their URI matching language.

It turns out,
there is a standard for URI Templates:
https://datatracker.ietf.org/doc/html/rfc6570
It's particularly comprehensive,
and covers all kinds of ways that parameters are encoded in URIs.

I'd wondered, could we also use the RFC 6570 template language
to describe URI matching?
