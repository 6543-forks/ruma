//! Implementation of event enum and event content enum macros.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, ToTokens};
use syn::{Attribute, Ident, LitStr};

use crate::event_parse::{EventEnumEntry, EventEnumInput, EventKind, EventKindVariation};

fn is_non_stripped_room_event(kind: &EventKind, var: &EventKindVariation) -> bool {
    matches!(kind, EventKind::Message | EventKind::State)
        && matches!(
            var,
            EventKindVariation::Full
                | EventKindVariation::Sync
                | EventKindVariation::Redacted
                | EventKindVariation::RedactedSync
        )
}

fn has_prev_content_field(kind: &EventKind, var: &EventKindVariation) -> bool {
    matches!(kind, EventKind::State)
        && matches!(var, EventKindVariation::Full | EventKindVariation::Sync)
}

type EventKindFn = fn(&EventKind, &EventKindVariation) -> bool;

/// This const is used to generate the accessor methods for the `Any*Event` enums.
///
/// DO NOT alter the field names unless the structs in `ruma_events::event_kinds` have changed.
const EVENT_FIELDS: &[(&str, EventKindFn)] = &[
    ("origin_server_ts", is_non_stripped_room_event),
    ("room_id", |kind, var| {
        matches!(kind, EventKind::Message | EventKind::State)
            && matches!(var, EventKindVariation::Full | EventKindVariation::Redacted)
    }),
    ("event_id", is_non_stripped_room_event),
    ("sender", |kind, &var| {
        matches!(kind, EventKind::Message | EventKind::State | EventKind::ToDevice)
            && var != EventKindVariation::Initial
    }),
    ("state_key", |kind, _| matches!(kind, EventKind::State)),
    ("unsigned", is_non_stripped_room_event),
];

/// Create a content enum from `EventEnumInput`.
pub fn expand_event_enum(input: EventEnumInput) -> syn::Result<TokenStream> {
    let import_path = crate::import_ruma_events();

    let name = &input.name;
    let attrs = &input.attrs;
    let events: Vec<_> = input.events.iter().map(|entry| entry.ev_type.clone()).collect();
    let variants: Vec<_> =
        input.events.iter().map(EventEnumEntry::to_variant).collect::<syn::Result<_>>()?;

    let event_enum = expand_any_with_deser(
        name,
        &events,
        attrs,
        &variants,
        &EventKindVariation::Full,
        &import_path,
    );

    let sync_event_enum = expand_any_with_deser(
        name,
        &events,
        attrs,
        &variants,
        &EventKindVariation::Sync,
        &import_path,
    );

    let stripped_event_enum = expand_any_with_deser(
        name,
        &events,
        attrs,
        &variants,
        &EventKindVariation::Stripped,
        &import_path,
    );

    let initial_event_enum = expand_any_with_deser(
        name,
        &events,
        attrs,
        &variants,
        &EventKindVariation::Initial,
        &import_path,
    );

    let redacted_event_enums = expand_any_redacted(name, &events, attrs, &variants, &import_path);

    let event_content_enum = expand_content_enum(name, &events, attrs, &variants, &import_path);

    Ok(quote! {
        #event_enum

        #sync_event_enum

        #stripped_event_enum

        #initial_event_enum

        #redacted_event_enums

        #event_content_enum
    })
}

fn expand_any_with_deser(
    kind: &EventKind,
    events: &[LitStr],
    attrs: &[Attribute],
    variants: &[EventEnumVariant],
    var: &EventKindVariation,
    import_path: &TokenStream,
) -> Option<TokenStream> {
    // If the event cannot be generated this bails out returning None which is rendered the same
    // as an empty `TokenStream`. This is effectively the check if the given input generates
    // a valid event enum.
    let (event_struct, ident) = generate_event_idents(kind, var)?;

    let content = events
        .iter()
        .map(|event| to_event_path(event, &event_struct, import_path))
        .collect::<Vec<_>>();

    let (custom_variant, custom_deserialize) =
        generate_custom_variant(&event_struct, var, import_path);

    let any_enum = quote! {
        #( #attrs )*
        #[derive(Clone, Debug, #import_path::exports::serde::Serialize)]
        #[serde(untagged)]
        #[allow(clippy::large_enum_variant)]
        pub enum #ident {
            #(
                #[doc = #events]
                #variants(#content),
            )*
            #custom_variant
        }
    };

    let event_deserialize_impl = quote! {
        impl<'de> #import_path::exports::serde::de::Deserialize<'de> for #ident {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: #import_path::exports::serde::de::Deserializer<'de>,
            {
                use #import_path::exports::serde::de::Error as _;

                let json = Box::<#import_path::exports::serde_json::value::RawValue>::deserialize(deserializer)?;
                let #import_path::EventDeHelper { ev_type, .. } =
                    #import_path::from_raw_json_value(&json)?;

                match ev_type.as_str() {
                    #(
                        #events => {
                            let event = #import_path::exports::serde_json::from_str::<#content>(json.get())
                                .map_err(D::Error::custom)?;
                            Ok(Self::#variants(event))
                        },
                    )*
                    #custom_deserialize
                }
            }
        }
    };

    let event_enum_to_from_sync = expand_conversion_impl(kind, var, &variants, import_path);

    let redacted_enum = expand_redacted_enum(kind, var, import_path);

    let field_accessor_impl = accessor_methods(kind, var, &variants, import_path);

    let redact_impl = expand_redact(&ident, kind, var, &variants, import_path);

    Some(quote! {
        #any_enum

        #event_enum_to_from_sync

        #field_accessor_impl

        #redact_impl

        #event_deserialize_impl

        #redacted_enum
    })
}

fn expand_conversion_impl(
    kind: &EventKind,
    var: &EventKindVariation,
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> Option<TokenStream> {
    let ident = kind.to_event_enum_ident(var)?;
    let variants = &variants
        .iter()
        .filter(|v| {
            // We filter this variant out only for non redacted events.
            // The type of the struct held in the enum variant is different in this case
            // so we construct the variant manually.
            !(v.ident == "RoomRedaction"
                && matches!(var, EventKindVariation::Full | EventKindVariation::Sync))
        })
        .collect::<Vec<_>>();

    match var {
        EventKindVariation::Full | EventKindVariation::Redacted => {
            // the opposite event variation full -> sync, redacted -> redacted sync
            let variation = if var == &EventKindVariation::Full {
                EventKindVariation::Sync
            } else {
                EventKindVariation::RedactedSync
            };

            let sync = kind.to_event_enum_ident(&variation)?;
            let sync_struct = kind.to_event_ident(&variation)?;

            let redaction = if let (EventKind::Message, EventKindVariation::Full) = (kind, var) {
                quote! {
                    #ident::RoomRedaction(event) => Self::RoomRedaction(
                        #import_path::room::redaction::SyncRedactionEvent::from(event),
                    ),
                }
            } else {
                TokenStream::new()
            };

            Some(quote! {
                impl From<#ident> for #sync {
                    fn from(event: #ident) -> Self {
                        match event {
                            #(
                                #ident::#variants(event) => {
                                    Self::#variants(#import_path::#sync_struct::from(event))
                                },
                            )*
                            #redaction
                            #ident::Custom(event) => {
                                Self::Custom(#import_path::#sync_struct::from(event))
                            },
                        }
                    }
                }
            })
        }
        EventKindVariation::Sync | EventKindVariation::RedactedSync => {
            let variation = if var == &EventKindVariation::Sync {
                EventKindVariation::Full
            } else {
                EventKindVariation::Redacted
            };
            let full = kind.to_event_enum_ident(&variation)?;

            let redaction = if let (EventKind::Message, EventKindVariation::Sync) = (kind, var) {
                quote! {
                    Self::RoomRedaction(event) => {
                        #full::RoomRedaction(event.into_full_event(room_id))
                    },
                }
            } else {
                TokenStream::new()
            };

            Some(quote! {
                impl #ident {
                    /// Convert this sync event into a full event, one with a room_id field.
                    pub fn into_full_event(self, room_id: #import_path::exports::ruma_identifiers::RoomId) -> #full {
                        match self {
                            #(
                                Self::#variants(event) => {
                                    #full::#variants(event.into_full_event(room_id))
                                },
                            )*
                            #redaction
                            Self::Custom(event) => {
                                #full::Custom(event.into_full_event(room_id))
                            },
                        }
                    }
                }
            })
        }
        _ => None,
    }
}

/// Generates the 3 redacted state enums, 2 redacted message enums,
/// and `Deserialize` implementations.
///
/// No content enums are generated since no part of the API deals with
/// redacted event's content. There are only five state variants that contain content.
fn expand_any_redacted(
    kind: &EventKind,
    events: &[LitStr],
    attrs: &[Attribute],
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> TokenStream {
    use EventKindVariation as V;

    if kind.is_state() {
        let full_state =
            expand_any_with_deser(kind, events, attrs, variants, &V::Redacted, import_path);
        let sync_state =
            expand_any_with_deser(kind, events, attrs, variants, &V::RedactedSync, import_path);
        let stripped_state =
            expand_any_with_deser(kind, events, attrs, variants, &V::RedactedStripped, import_path);

        quote! {
            #full_state

            #sync_state

            #stripped_state
        }
    } else if kind.is_message() {
        let full_message =
            expand_any_with_deser(kind, events, attrs, variants, &V::Redacted, import_path);
        let sync_message =
            expand_any_with_deser(kind, events, attrs, variants, &V::RedactedSync, import_path);

        quote! {
            #full_message

            #sync_message
        }
    } else {
        TokenStream::new()
    }
}

/// Create a content enum from `EventEnumInput`.
fn expand_content_enum(
    kind: &EventKind,
    events: &[LitStr],
    attrs: &[Attribute],
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> TokenStream {
    let ident = kind.to_content_enum();
    let event_type_str = events;

    let content =
        events.iter().map(|ev| to_event_content_path(ev, import_path)).collect::<Vec<_>>();

    let content_enum = quote! {
        #( #attrs )*
        #[derive(Clone, Debug, #import_path::exports::serde::Serialize)]
        #[serde(untagged)]
        #[allow(clippy::large_enum_variant)]
        pub enum #ident {
            #(
                #[doc = #event_type_str]
                #variants(#content),
            )*
            /// Content of an event not defined by the Matrix specification.
            Custom(#import_path::custom::CustomEventContent),
        }
    };

    let event_content_impl = quote! {
        impl #import_path::EventContent for #ident {
            fn event_type(&self) -> &str {
                match self {
                    #( Self::#variants(content) => content.event_type(), )*
                    Self::Custom(content) => content.event_type(),
                }
            }

            fn from_parts(
                event_type: &str, input: Box<#import_path::exports::serde_json::value::RawValue>,
            ) -> Result<Self, #import_path::exports::serde_json::Error> {
                match event_type {
                    #(
                        #event_type_str => {
                            let content = #content::from_parts(event_type, input)?;
                            Ok(Self::#variants(content))
                        },
                    )*
                    ev_type => {
                        let content =
                            #import_path::custom::CustomEventContent::from_parts(ev_type, input)?;
                        Ok(Self::Custom(content))
                    },
                }
            }
        }
    };

    let marker_trait_impls = marker_traits(&kind, import_path);

    quote! {
        #content_enum

        #event_content_impl

        #marker_trait_impls
    }
}

fn expand_redact(
    ident: &Ident,
    kind: &EventKind,
    var: &EventKindVariation,
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> Option<TokenStream> {
    if let EventKindVariation::Full | EventKindVariation::Sync | EventKindVariation::Stripped = var
    {
        let (param, redaction_type, redaction_enum) = match var {
            EventKindVariation::Full => {
                let struct_id = kind.to_event_ident(&EventKindVariation::Redacted)?;
                (
                    quote! { #import_path::room::redaction::RedactionEvent },
                    quote! { #import_path::#struct_id },
                    kind.to_event_enum_ident(&EventKindVariation::Redacted)?,
                )
            }
            EventKindVariation::Sync => {
                let struct_id = kind.to_event_ident(&EventKindVariation::RedactedSync)?;
                (
                    quote! { #import_path::room::redaction::SyncRedactionEvent },
                    quote! { #import_path::#struct_id },
                    kind.to_event_enum_ident(&EventKindVariation::RedactedSync)?,
                )
            }
            EventKindVariation::Stripped => {
                let struct_id = kind.to_event_ident(&EventKindVariation::RedactedStripped)?;
                (
                    quote! { #import_path::room::redaction::SyncRedactionEvent },
                    quote! { #import_path::#struct_id },
                    kind.to_event_enum_ident(&EventKindVariation::RedactedStripped)?,
                )
            }
            _ => return None,
        };

        let fields = EVENT_FIELDS.iter().map(|(name, has_field)| {
            generate_redacted_fields(name, kind, var, *has_field, import_path)
        });

        let fields = quote! { #( #fields )* };

        Some(quote! {
            impl #ident {
                /// Redacts `Self` given a valid `Redaction[Sync]Event`.
                pub fn redact(
                    self,
                    redaction: #param,
                    version: #import_path::exports::ruma_identifiers::RoomVersionId,
                ) -> #redaction_enum {
                    match self {
                        #(
                            Self::#variants(event) => {
                                let content = event.content.redact(version);
                                #redaction_enum::#variants(#redaction_type {
                                    content,
                                    #fields
                                })
                            }
                        )*
                        Self::Custom(event) => {
                            let content = event.content.redact(version);
                            #redaction_enum::Custom(#redaction_type {
                                content,
                                #fields
                            })
                        }
                    }
                }
            }
        })
    } else {
        None
    }
}

fn expand_redacted_enum(
    kind: &EventKind,
    var: &EventKindVariation,
    import_path: &TokenStream,
) -> Option<TokenStream> {
    if let EventKind::State | EventKind::Message = kind {
        let ident = format_ident!("AnyPossiblyRedacted{}", kind.to_event_ident(var)?);

        // FIXME: Use Option::zip once MSRV >= 1.46
        let (regular_enum_ident, redacted_enum_ident) = match inner_enum_idents(kind, var) {
            (Some(regular_enum_ident), Some(redacted_enum_ident)) => {
                (regular_enum_ident, redacted_enum_ident)
            }
            _ => return None,
        };

        Some(quote! {
            /// An enum that holds either regular un-redacted events or redacted events.
            #[derive(Clone, Debug, #import_path::exports::serde::Serialize)]
            #[serde(untagged)]
            pub enum #ident {
                /// An un-redacted event.
                Regular(#regular_enum_ident),

                /// A redacted event.
                Redacted(#redacted_enum_ident),
            }

            impl<'de> #import_path::exports::serde::de::Deserialize<'de> for #ident {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: #import_path::exports::serde::de::Deserializer<'de>,
                {
                    let json = Box::<#import_path::exports::serde_json::value::RawValue>::deserialize(deserializer)?;
                    let #import_path::EventDeHelper { unsigned, .. } =
                        #import_path::from_raw_json_value(&json)?;

                    Ok(match unsigned {
                        Some(unsigned) if unsigned.redacted_because.is_some() => {
                            Self::Redacted(#import_path::from_raw_json_value(&json)?)
                        }
                        _ => Self::Regular(#import_path::from_raw_json_value(&json)?),
                    })
                }
            }
        })
    } else {
        None
    }
}

fn generate_event_idents(kind: &EventKind, var: &EventKindVariation) -> Option<(Ident, Ident)> {
    Some((kind.to_event_ident(var)?, kind.to_event_enum_ident(var)?))
}

fn generate_redacted_fields(
    name: &str,
    kind: &EventKind,
    var: &EventKindVariation,
    is_event_kind: EventKindFn,
    import_path: &TokenStream,
) -> TokenStream {
    if is_event_kind(kind, var) {
        let name = Ident::new(name, Span::call_site());

        if name == "unsigned" {
            let redaction_type = if let EventKindVariation::Sync = var {
                quote! { RedactedSyncUnsigned }
            } else {
                quote! { RedactedUnsigned }
            };

            quote! {
                unsigned: #import_path::#redaction_type {
                    redacted_because: Some(::std::boxed::Box::new(redaction)),
                },
            }
        } else {
            quote! {
                #name: event.#name,
            }
        }
    } else {
        TokenStream::new()
    }
}

fn generate_custom_variant(
    event_struct: &Ident,
    var: &EventKindVariation,
    import_path: &TokenStream,
) -> (TokenStream, TokenStream) {
    use EventKindVariation as V;

    if matches!(var, V::Redacted | V::RedactedSync | V::RedactedStripped) {
        (
            quote! {
                /// A redacted event not defined by the Matrix specification
                Custom(#import_path::#event_struct<#import_path::custom::RedactedCustomEventContent>),
            },
            quote! {
                event => {
                    let event = #import_path::exports::serde_json::from_str::<
                        #import_path::#event_struct<#import_path::custom::RedactedCustomEventContent>,
                    >(json.get())
                    .map_err(D::Error::custom)?;

                    Ok(Self::Custom(event))
                },
            },
        )
    } else {
        (
            quote! {
                /// An event not defined by the Matrix specification
                Custom(#import_path::#event_struct<#import_path::custom::CustomEventContent>),
            },
            quote! {
                event => {
                    let event =
                        #import_path::exports::serde_json::from_str::<
                            #import_path::#event_struct<#import_path::custom::CustomEventContent>
                        >(json.get())
                        .map_err(D::Error::custom)?;

                    Ok(Self::Custom(event))
                },
            },
        )
    }
}

fn marker_traits(kind: &EventKind, import_path: &TokenStream) -> TokenStream {
    let ident = kind.to_content_enum();
    match kind {
        EventKind::State => quote! {
            impl #import_path::RoomEventContent for #ident {}
            impl #import_path::StateEventContent for #ident {}
        },
        EventKind::Message => quote! {
            impl #import_path::RoomEventContent for #ident {}
            impl #import_path::MessageEventContent for #ident {}
        },
        EventKind::Ephemeral => quote! {
            impl #import_path::EphemeralRoomEventContent for #ident {}
        },
        EventKind::Basic => quote! {
            impl #import_path::BasicEventContent for #ident {}
        },
        _ => TokenStream::new(),
    }
}

fn accessor_methods(
    kind: &EventKind,
    var: &EventKindVariation,
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> Option<TokenStream> {
    use EventKindVariation as V;

    let ident = kind.to_event_enum_ident(var)?;

    // matching `EventKindVariation`s
    if let V::Redacted | V::RedactedSync | V::RedactedStripped = var {
        return redacted_accessor_methods(kind, var, variants, import_path);
    }

    let methods = EVENT_FIELDS.iter().map(|(name, has_field)| {
        generate_accessor(name, kind, var, *has_field, variants, import_path)
    });

    let content_enum = kind.to_content_enum();

    let content = quote! {
        /// Returns the any content enum for this event.
        pub fn content(&self) -> #content_enum {
            match self {
                #(
                    Self::#variants(event) => #content_enum::#variants(event.content.clone()),
                )*
                Self::Custom(event) => #content_enum::Custom(event.content.clone()),
            }
        }
    };

    let prev_content = if has_prev_content_field(kind, var) {
        quote! {
            /// Returns the any content enum for this events prev_content.
            pub fn prev_content(&self) -> Option<#content_enum> {
                match self {
                    #(
                        Self::#variants(event) => {
                            event.prev_content.as_ref().map(|c| #content_enum::#variants(c.clone()))
                        },
                    )*
                    Self::Custom(event) => {
                        event.prev_content.as_ref().map(|c| #content_enum::Custom(c.clone()))
                    },
                }
            }
        }
    } else {
        TokenStream::new()
    };

    Some(quote! {
        impl #ident {
            #content

            #prev_content

            #( #methods )*
        }
    })
}

fn inner_enum_idents(kind: &EventKind, var: &EventKindVariation) -> (Option<Ident>, Option<Ident>) {
    match var {
        EventKindVariation::Full => {
            (kind.to_event_enum_ident(var), kind.to_event_enum_ident(&EventKindVariation::Redacted))
        }
        EventKindVariation::Sync => (
            kind.to_event_enum_ident(var),
            kind.to_event_enum_ident(&EventKindVariation::RedactedSync),
        ),
        EventKindVariation::Stripped => (
            kind.to_event_enum_ident(var),
            kind.to_event_enum_ident(&EventKindVariation::RedactedStripped),
        ),
        EventKindVariation::Initial => (kind.to_event_enum_ident(var), None),
        _ => (None, None),
    }
}

/// Redacted events do NOT generate `content` or `prev_content` methods like
/// un-redacted events; otherwise, they are the same.
fn redacted_accessor_methods(
    kind: &EventKind,
    var: &EventKindVariation,
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> Option<TokenStream> {
    // this will never fail as it is called in `expand_any_with_deser`.
    let ident = kind.to_event_enum_ident(var).unwrap();
    let methods = EVENT_FIELDS.iter().map(|(name, has_field)| {
        generate_accessor(name, kind, var, *has_field, variants, import_path)
    });

    Some(quote! {
        impl #ident {
            #( #methods )*
        }
    })
}

fn to_event_path(name: &LitStr, struct_name: &Ident, import_path: &TokenStream) -> TokenStream {
    let span = name.span();
    let name = name.value();

    // There is no need to give a good compiler error as `to_camel_case` is called first.
    assert_eq!(&name[..2], "m.");

    let path = name[2..].split('.').collect::<Vec<_>>();

    let event_str = path.last().unwrap();
    let event = event_str
        .split('_')
        .map(|s| s.chars().next().unwrap().to_uppercase().to_string() + &s[1..])
        .collect::<String>();

    let path = path.iter().map(|s| Ident::new(s, span));

    match struct_name.to_string().as_str() {
        "MessageEvent" | "SyncMessageEvent" if name == "m.room.redaction" => {
            let redaction = if struct_name == "MessageEvent" {
                quote! { RedactionEvent }
            } else {
                quote! { SyncRedactionEvent }
            };
            quote! { #import_path::room::redaction::#redaction }
        }
        "ToDeviceEvent"
        | "SyncStateEvent"
        | "StrippedStateEvent"
        | "InitialStateEvent"
        | "SyncMessageEvent"
        | "SyncEphemeralRoomEvent" => {
            let content = format_ident!("{}EventContent", event);
            quote! { #import_path::#struct_name<#import_path::#( #path )::*::#content> }
        }
        struct_str if struct_str.contains("Redacted") => {
            let content = format_ident!("Redacted{}EventContent", event);
            quote! { #import_path::#struct_name<#import_path::#( #path )::*::#content> }
        }
        _ => {
            let event_name = format_ident!("{}Event", event);
            quote! { #import_path::#( #path )::*::#event_name }
        }
    }
}

fn to_event_content_path(name: &LitStr, import_path: &TokenStream) -> TokenStream {
    let span = name.span();
    let name = name.value();

    // There is no need to give a good compiler error as `to_camel_case` is called first.
    assert_eq!(&name[..2], "m.");

    let path = name[2..].split('.').collect::<Vec<_>>();

    let event_str = path.last().unwrap();
    let event = event_str
        .split('_')
        .map(|s| s.chars().next().unwrap().to_uppercase().to_string() + &s[1..])
        .collect::<String>();

    let content_str = format_ident!("{}EventContent", event);
    let path = path.iter().map(|s| Ident::new(s, span));
    quote! {
        #import_path::#( #path )::*::#content_str
    }
}

/// Splits the given `event_type` string on `.` and `_` removing the `m.room.` then
/// camel casing to give the `Event` struct name.
fn to_camel_case(name: &LitStr) -> syn::Result<Ident> {
    let span = name.span();
    let name = name.value();

    if &name[..2] != "m." {
        return Err(syn::Error::new(
            span,
            format!("well-known matrix events have to start with `m.` found `{}`", name),
        ));
    }

    let s = name[2..]
        .split(&['.', '_'] as &[char])
        .map(|s| s.chars().next().unwrap().to_uppercase().to_string() + &s[1..])
        .collect::<String>();

    Ok(Ident::new(&s, span))
}

fn generate_accessor(
    name: &str,
    kind: &EventKind,
    var: &EventKindVariation,
    is_event_kind: EventKindFn,
    variants: &[EventEnumVariant],
    import_path: &TokenStream,
) -> TokenStream {
    if is_event_kind(kind, var) {
        let field_type = field_return_type(name, var, import_path);

        let name = Ident::new(name, Span::call_site());
        let docs = format!("Returns this events {} field.", name);
        quote! {
            #[doc = #docs]
            pub fn #name(&self) -> &#field_type {
                match self {
                    #(
                        Self::#variants(event) => &event.#name,
                    )*
                    Self::Custom(event) => &event.#name,
                }
            }
        }
    } else {
        TokenStream::new()
    }
}

fn field_return_type(
    name: &str,
    var: &EventKindVariation,
    import_path: &TokenStream,
) -> TokenStream {
    match name {
        "origin_server_ts" => quote! { ::std::time::SystemTime },
        "room_id" => quote! { #import_path::exports::ruma_identifiers::RoomId },
        "event_id" => quote! { #import_path::exports::ruma_identifiers::EventId },
        "sender" => quote! { #import_path::exports::ruma_identifiers::UserId },
        "state_key" => quote! { str },
        "unsigned" if &EventKindVariation::RedactedSync == var => {
            quote! { #import_path::RedactedSyncUnsigned }
        }
        "unsigned" if &EventKindVariation::Redacted == var => {
            quote! { #import_path::RedactedUnsigned }
        }
        "unsigned" => quote! { #import_path::Unsigned },
        _ => panic!("the `ruma_events_macros::event_enum::EVENT_FIELD` const was changed"),
    }
}

struct EventEnumVariant {
    pub attrs: Vec<Attribute>,
    pub ident: Ident,
}

impl ToTokens for EventEnumVariant {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        for attr in &self.attrs {
            attr.to_tokens(tokens);
        }
        self.ident.to_tokens(tokens);
    }
}

impl EventEnumEntry {
    fn to_variant(&self) -> syn::Result<EventEnumVariant> {
        let attrs = self.attrs.clone();
        let ident = to_camel_case(&self.ev_type)?;
        Ok(EventEnumVariant { attrs, ident })
    }
}
