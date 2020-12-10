// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, collections::HashSet, fmt::Display};

use crate::{
    additional_cpp_generator::AdditionalNeed, byvalue_checker::ByValueChecker,
    byvalue_scanner::identify_byvalue_safe_types, type_database::TypeDatabase, types::make_ident,
    types::Namespace, types::TypeName,
};
use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::{
    parse::Parser, parse_quote, Field, Fields, ForeignItem, GenericParam, Ident, Item,
    ItemForeignMod, ItemMod, ItemStruct, Type,
};

use super::{
    bridge_name_tracker::BridgeNameTracker,
    foreign_mod_converter::{ForeignModConversionCallbacks, ForeignModConverter},
    namespace_organizer::NamespaceEntries,
    rust_name_tracker::RustNameTracker,
    type_converter::TypeConverter,
    utilities::generate_utilities,
};

unzip_n::unzip_n!(pub 4);

#[derive(Debug)]
pub enum ConvertError {
    NoContent,
    UnsafePODType(String),
    UnexpectedForeignItem,
    UnexpectedOuterItem,
    UnexpectedItemInMod,
    ComplexTypedefTarget(String),
    UnexpectedThisType,
}

impl Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::NoContent => write!(f, "The initial run of 'bindgen' did not generate any content. This might be because none of the requested items for generation could be converted.")?,
            ConvertError::UnsafePODType(err) => write!(f, "An item was requested using 'generate_pod' which was not safe to hold by value in Rust. {}", err)?,
            ConvertError::UnexpectedForeignItem => write!(f, "Bindgen generated some unexpected code in a foreign mod section. You may have specified something in a 'generate' directive which is not currently compatible with autocxx.")?,
            ConvertError::UnexpectedOuterItem => write!(f, "Bindgen generated some unexpected code in its outermost mod section. You may have specified something in a 'generate' directive which is not currently compatible with autocxx.")?,
            ConvertError::UnexpectedItemInMod => write!(f, "Bindgen generated some unexpected code in an inner namespace mod. You may have specified something in a 'generate' directive which is not currently compatible with autocxx.")?,
            ConvertError::ComplexTypedefTarget(ty) => write!(f, "autocxx was unable to produce a typdef pointing to the complex type {}.", ty)?,
            ConvertError::UnexpectedThisType => write!(f, "Unexpected type for 'this'")?, // TODO give type/function
        }
        Ok(())
    }
}

/// Whetther and how this type should be exposed in the mods constructed
/// for actual end-user use.
pub(crate) enum Use {
    Unused,
    Used,
    UsedWithAlias(Ident),
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum TypeKind {
    POD,                // trivial. Can be moved and copied in Rust.
    NonPOD, // has destructor or non-trivial move constructors. Can only hold by UniquePtr
    ForwardDeclaration, // no full C++ declaration available - can't even generate UniquePtr
}

/// Any API we encounter in the input bindgen rs which we might want to pass
/// onto the output Rust or C++. Everything is stored in these structures
/// because we will do a garbage collection for unnecessary APIs later,
/// using the `deps` field as the edges in the graph.
pub(crate) struct Api {
    pub(crate) ns: Namespace,
    pub(crate) id: Ident,
    pub(crate) use_stmt: Use,
    pub(crate) deps: HashSet<TypeName>,
    pub(crate) extern_c_mod_item: Option<ForeignItem>,
    pub(crate) bridge_item: Option<Item>,
    pub(crate) global_items: Vec<Item>,
    pub(crate) additional_cpp: Option<AdditionalNeed>,
    pub(crate) id_for_allowlist: Option<Ident>,
    pub(crate) bindgen_mod_item: Option<Item>,
}

impl Api {
    fn typename(&self) -> TypeName {
        TypeName::new(&self.ns, &self.id.to_string())
    }

    fn typename_for_allowlist(&self) -> TypeName {
        let id_for_allowlist = match &self.id_for_allowlist {
            None => match &self.use_stmt {
                Use::UsedWithAlias(alias) => alias,
                _ => &self.id,
            },
            Some(id) => &id,
        };
        TypeName::new(&self.ns, &id_for_allowlist.to_string())
    }
}

/// Results of a conversion.
pub(crate) struct BridgeConversionResults {
    pub items: Vec<Item>,
    pub additional_cpp_needs: Vec<AdditionalNeed>,
}

/// Converts the bindings generated by bindgen into a form suitable
/// for use with `cxx`.
/// In fact, most of the actual operation happens within an
/// individual `BridgeConversion`.
///
/// # Flexibility in handling bindgen output
///
/// autocxx is inevitably tied to the details of the bindgen output;
/// e.g. the creation of a 'root' mod when namespaces are enabled.
/// At the moment this crate takes the view that it's OK to panic
/// if the bindgen output is not as expected. It may be in future that
/// we need to be a bit more graceful, but for now, that's OK.
pub(crate) struct BridgeConverter<'a> {
    include_list: &'a [String],
    type_database: &'a TypeDatabase,
}

impl<'a> BridgeConverter<'a> {
    pub fn new(include_list: &'a [String], type_database: &'a TypeDatabase) -> Self {
        Self {
            include_list,
            type_database,
        }
    }

    /// Convert a TokenStream of bindgen-generated bindings to a form
    /// suitable for cxx.
    pub(crate) fn convert(
        &mut self,
        bindings: ItemMod,
        exclude_utilities: bool,
    ) -> Result<BridgeConversionResults, ConvertError> {
        match bindings.content {
            None => Err(ConvertError::NoContent),
            Some((brace, items)) => {
                let bindgen_mod = ItemMod {
                    attrs: bindings.attrs,
                    vis: bindings.vis,
                    ident: bindings.ident,
                    mod_token: bindings.mod_token,
                    content: Some((brace, Vec::new())),
                    semi: bindings.semi,
                };
                let items_in_root = find_items_in_root(items)?;
                let byvalue_checker =
                    identify_byvalue_safe_types(&items_in_root, &self.type_database)?;
                let conversion = BridgeConversion {
                    bindgen_mod,
                    extern_c_mod: None,
                    type_converter: TypeConverter::new(),
                    byvalue_checker,
                    include_list: &self.include_list,
                    apis: Vec::new(),
                    bridge_name_tracker: BridgeNameTracker::new(),
                    rust_name_tracker: RustNameTracker::new(),
                    type_database: &self.type_database,
                    use_stmts_by_mod: HashMap::new(),
                    incomplete_types: HashSet::new(),
                };
                conversion.convert_items(items_in_root, exclude_utilities)
            }
        }
    }
}

fn get_blank_extern_c_mod() -> ItemForeignMod {
    parse_quote!(
        extern "C" {}
    )
}

/// A particular bridge conversion operation. This can really
/// be thought of as a ton of parameters which we'd otherwise
/// need to pass into each individual function within this file.
struct BridgeConversion<'a> {
    bindgen_mod: ItemMod,
    extern_c_mod: Option<ItemForeignMod>,
    type_converter: TypeConverter,
    byvalue_checker: ByValueChecker,
    include_list: &'a [String],
    type_database: &'a TypeDatabase,
    apis: Vec<Api>,
    bridge_name_tracker: BridgeNameTracker,
    rust_name_tracker: RustNameTracker,
    use_stmts_by_mod: HashMap<Namespace, Vec<Item>>,
    incomplete_types: HashSet<TypeName>,
}

fn remove_nones<T>(input: Vec<Option<T>>) -> Vec<T> {
    input.into_iter().flatten().collect()
}

fn find_items_in_root(items: Vec<Item>) -> Result<Vec<Item>, ConvertError> {
    for item in items {
        match item {
            Item::Mod(root_mod) => {
                // With namespaces enabled, bindgen always puts everything
                // in a mod called 'root'. We don't want to pass that
                // onto cxx, so jump right into it.
                assert!(root_mod.ident == "root");
                if let Some((_, items)) = root_mod.content {
                    return Ok(items);
                }
            }
            _ => return Err(ConvertError::UnexpectedOuterItem),
        }
    }
    Ok(Vec::new())
}

impl<'a> BridgeConversion<'a> {
    /// Main function which goes through and performs conversion from
    /// `bindgen`-style Rust output into `cxx::bridge`-style Rust input.
    /// At present, it significantly rewrites the bindgen mod,
    /// as well as generating an additional cxx::bridge mod, and an outer
    /// mod with all sorts of 'use' statements. A valid alternative plan
    /// might be to keep the bindgen mod untouched and _only_ generate
    /// additional bindings, but the sticking point there is that it's not
    /// obviously possible to stop folks allocating opaque types in the
    /// bindgen mod. (We mark all types as opaque until we're told
    /// otherwise, which is the opposite of what bindgen does, so we can't
    /// just give it lots of directives to make all types opaque.)
    /// One future option could be to provide a mode to bindgen where
    /// everything is opaque unless specifically allowlisted to be
    /// transparent.
    fn convert_items(
        mut self,
        items: Vec<Item>,
        exclude_utilities: bool,
    ) -> Result<BridgeConversionResults, ConvertError> {
        if !exclude_utilities {
            generate_utilities(&mut self.apis);
        }
        let root_ns = Namespace::new();
        self.convert_mod_items(items, root_ns)?;
        // The code above will have contributed lots of Apis to self.apis.
        // We now garbage collect the ones we don't need...
        let all_apis = self.filter_apis_by_following_edges_from_allowlist();
        // ... and now let's start to generate the output code.
        // First, the hierarchy of mods containing lots of 'use' statements
        // which is the final API exposed as 'ffi'.
        let mut use_statements = Self::generate_final_use_statements(&all_apis);
        // Next, the (modified) bindgen output, which we include in the
        // output as a 'bindgen' sub-mod.
        let bindgen_root_items = self.generate_final_bindgen_mods(&all_apis);
        // Both of the above are organized into sub-mods by namespace.
        // From here on, things are flat.
        let (extern_c_mod_items, all_items, bridge_items, additional_cpp_needs) = all_apis
            .into_iter()
            .map(|api| {
                (
                    api.extern_c_mod_item,
                    api.global_items,
                    api.bridge_item,
                    api.additional_cpp,
                )
            })
            .unzip_n_vec();
        // Items for the [cxx::bridge] mod...
        let mut bridge_items = remove_nones(bridge_items);
        // Things to include in the "extern "C"" mod passed within the cxx::bridge
        let mut extern_c_mod_items = remove_nones(extern_c_mod_items);
        // And a list of global items to include at the top level.
        let mut all_items: Vec<Item> = all_items.into_iter().flatten().collect();
        // And finally any C++ we need to generate. And by "we" I mean autocxx not cxx.
        let additional_cpp_needs = remove_nones(additional_cpp_needs);
        extern_c_mod_items
            .extend(self.build_include_foreign_items(!additional_cpp_needs.is_empty()));
        // We will always create an extern "C" mod even if bindgen
        // didn't generate one, e.g. because it only generated types.
        // We still want cxx to know about those types.
        let mut extern_c_mod = self
            .extern_c_mod
            .take()
            .unwrap_or_else(get_blank_extern_c_mod);
        extern_c_mod.items.append(&mut extern_c_mod_items);
        bridge_items.push(Item::ForeignMod(extern_c_mod));
        // The extensive use of parse_quote here could end up
        // being a performance bottleneck. If so, we might want
        // to set the 'contents' field of the ItemMod
        // structures directly.
        if !bindgen_root_items.is_empty() {
            self.bindgen_mod.content.as_mut().unwrap().1 = vec![Item::Mod(parse_quote! {
                pub mod root {
                    #(#bindgen_root_items)*
                }
            })];
            all_items.push(Item::Mod(self.bindgen_mod));
        }
        all_items.push(Item::Mod(parse_quote! {
            #[cxx::bridge]
            pub mod cxxbridge {
                #(#bridge_items)*
            }
        }));
        all_items.append(&mut use_statements);
        Ok(BridgeConversionResults {
            items: all_items,
            additional_cpp_needs,
        })
    }

    /// This is essentially mark-and-sweep garbage collection of the
    /// Apis that we've discovered. Why do we do this, you might wonder?
    /// It seems a bit strange given that we pass an explicit allowlist
    /// to bindgen.
    /// There are two circumstances under which we want to discard
    /// some of the APIs we encounter parsing the bindgen.
    /// 1) We simplify some struct to be non-POD. In this case, we'll
    ///    discard all the fields within it. Those fields can be, and
    ///    in fact often _are_, stuff which we have trouble converting
    ///    e.g. std::string or std::string::value_type or
    ///    my_derived_thing<std::basic_string::value_type> or some
    ///    other permutation. In such cases, we want to discard those
    ///    field types with prejudice.
    /// 2) block! may be used to ban certain APIs. This often eliminates
    ///    some methods from a given struct/class. In which case, we
    ///    don't care about the other parameter types passed into those
    ///    APIs either.
    fn filter_apis_by_following_edges_from_allowlist(&mut self) -> Vec<Api> {
        let mut todos: Vec<_> = self
            .apis
            .iter()
            .filter(|api| {
                let tnforal = api.typename_for_allowlist();
                log::info!("Considering {}", tnforal);
                self.is_on_allowlist(&tnforal)
            })
            .map(Api::typename)
            .collect();
        let mut by_typename: HashMap<TypeName, Vec<Api>> = HashMap::new();
        for api in self.apis.drain(..) {
            let tn = api.typename();
            by_typename.entry(tn).or_default().push(api);
        }
        let mut done = HashSet::new();
        let mut output = Vec::new();
        while !todos.is_empty() {
            let todo = todos.remove(0);
            if done.contains(&todo) {
                continue;
            }
            if let Some(mut these_apis) = by_typename.remove(&todo) {
                todos.extend(these_apis.iter_mut().flat_map(|api| api.deps.drain()));
                output.append(&mut these_apis);
            } // otherwise, probably an intrinsic e.g. uint32_t.
            done.insert(todo);
        }
        output
    }

    /// Interpret the bindgen-generated .rs for a particular
    /// mod, which corresponds to a C++ namespace.
    fn convert_mod_items(&mut self, items: Vec<Item>, ns: Namespace) -> Result<(), ConvertError> {
        // This object maintains some state specific to this namespace, i.e.
        // this particular mod.
        let mut mod_converter = ForeignModConverter::new(ns.clone());
        let mut use_statements_for_this_mod = Vec::new();
        for item in items {
            match item {
                Item::ForeignMod(mut fm) => {
                    let items = fm.items;
                    fm.items = Vec::new();
                    if self.extern_c_mod.is_none() {
                        self.extern_c_mod = Some(fm);
                        // We'll use the first 'extern "C"' mod we come
                        // across for attributes, spans etc. but we'll stuff
                        // the contents of all bindgen 'extern "C"' mods into this
                        // one.
                    }
                    mod_converter.convert_foreign_mod_items(items)?;
                }
                Item::Struct(mut s) => {
                    let tyname = TypeName::new(&ns, &s.ident.to_string());
                    let type_kind = if Self::spot_forward_declaration(&s.fields) {
                        self.incomplete_types.insert(tyname.clone());
                        TypeKind::ForwardDeclaration
                    } else {
                        if self.byvalue_checker.is_pod(&tyname) {
                            TypeKind::POD
                        } else {
                            TypeKind::NonPOD
                        }
                    };
                    // We either leave a bindgen struct untouched, or we completely
                    // replace its contents with opaque nonsense.
                    let field_types = match type_kind {
                        TypeKind::POD => self.get_struct_field_types(&ns, &s)?,
                        _ => {
                            Self::make_non_pod(&mut s);
                            HashSet::new()
                        }
                    };
                    // cxx::bridge can't cope with type aliases to generic
                    // types at the moment.
                    self.generate_type(tyname, type_kind, field_types, Some(Item::Struct(s)))?;
                }
                Item::Enum(e) => {
                    let tyname = TypeName::new(&ns, &e.ident.to_string());
                    self.generate_type(tyname, TypeKind::POD, HashSet::new(), Some(Item::Enum(e)))?;
                }
                Item::Impl(imp) => {
                    // We *mostly* ignore all impl blocks generated by bindgen.
                    // Methods also appear in 'extern "C"' blocks which
                    // we will convert instead. At that time we'll also construct
                    // synthetic impl blocks.
                    // We do however record which methods were spotted, since
                    // we have no other way of working out which functions are
                    // static methods vs plain functions.
                    mod_converter.convert_impl_items(imp);
                }
                Item::Mod(itm) => {
                    if let Some((_, items)) = itm.content {
                        let new_ns = ns.push(itm.ident.to_string());
                        self.convert_mod_items(items, new_ns)?;
                    }
                }
                Item::Use(_) => {
                    use_statements_for_this_mod.push(item);
                }
                Item::Const(itc) => {
                    // TODO the following puts this constant into
                    // the global namespace which is bug
                    // https://github.com/google/autocxx/issues/133
                    self.add_api(Api {
                        id: itc.ident.clone(),
                        ns: ns.clone(),
                        bridge_item: None,
                        extern_c_mod_item: None,
                        global_items: vec![Item::Const(itc)],
                        additional_cpp: None,
                        deps: HashSet::new(),
                        use_stmt: Use::Unused,
                        id_for_allowlist: None,
                        bindgen_mod_item: None,
                    });
                }
                Item::Type(ity) => {
                    let tyname = TypeName::new(&ns, &ity.ident.to_string());
                    self.type_converter.insert_typedef(tyname, ity.ty.as_ref());
                    self.add_api(Api {
                        id: ity.ident.clone(),
                        ns: ns.clone(),
                        bridge_item: None,
                        extern_c_mod_item: None,
                        global_items: Vec::new(),
                        additional_cpp: None,
                        deps: HashSet::new(),
                        use_stmt: Use::Unused,
                        id_for_allowlist: None,
                        bindgen_mod_item: Some(Item::Type(ity)),
                    });
                }
                _ => return Err(ConvertError::UnexpectedItemInMod),
            }
        }
        mod_converter.finished(self)?;

        // We don't immediately blat 'use' statements into any particular
        // Api. We'll squirrel them away and insert them into the output mod later
        // iff this mod ends up having any output items after garbage collection
        // of unnecessary APIs.
        let supers = std::iter::repeat(make_ident("super")).take(ns.depth() + 2);
        use_statements_for_this_mod.push(Item::Use(parse_quote! {
            #[allow(unused_imports)]
            use self::
                #(#supers)::*
            ::cxxbridge;
        }));
        for thing in &["UniquePtr", "CxxString"] {
            let thing = make_ident(thing);
            use_statements_for_this_mod.push(Item::Use(parse_quote! {
                #[allow(unused_imports)]
                use cxx:: #thing;
            }));
        }
        self.use_stmts_by_mod
            .insert(ns, use_statements_for_this_mod);
        Ok(())
    }

    fn get_struct_field_types(
        &self,
        ns: &Namespace,
        s: &ItemStruct,
    ) -> Result<HashSet<TypeName>, ConvertError> {
        let mut results = HashSet::new();
        for f in &s.fields {
            let annotated = self.type_converter.convert_type(f.ty.clone(), ns)?;
            results.extend(annotated.types_encountered);
        }
        Ok(results)
    }

    fn spot_forward_declaration(s: &Fields) -> bool {
        s.iter()
            .filter_map(|f| f.ident.as_ref())
            .any(|id| id == "_unused")
    }

    fn make_non_pod(s: &mut ItemStruct) {
        // Thanks to dtolnay@ for this explanation of why the following
        // is needed:
        // If the real alignment of the C++ type is smaller and a reference
        // is returned from C++ to Rust, mere existence of an insufficiently
        // aligned reference in Rust causes UB even if never dereferenced
        // by Rust code
        // (see https://doc.rust-lang.org/1.47.0/reference/behavior-considered-undefined.html).
        // Rustc can use least-significant bits of the reference for other storage.
        s.attrs = vec![parse_quote!(
            #[repr(C, packed)]
        )];
        // Now fill in fields. Usually, we just want a single field
        // but if this is a generic type we need to faff a bit.
        let generic_type_fields =
            s.generics
                .params
                .iter()
                .enumerate()
                .filter_map(|(counter, gp)| match gp {
                    GenericParam::Type(gpt) => {
                        let id = &gpt.ident;
                        let field_name = make_ident(&format!("_phantom_{}", counter));
                        let toks = quote! {
                            #field_name: ::std::marker::PhantomData<::std::cell::UnsafeCell< #id >>
                        };
                        let parser = Field::parse_named;
                        Some(parser.parse2(toks).unwrap())
                    }
                    _ => None,
                });
        // See cxx's opaque::Opaque for rationale for this type... in
        // short, it's to avoid being Send/Sync.
        s.fields = syn::Fields::Named(parse_quote! {
            {
                do_not_attempt_to_allocate_nonpod_types: [*const u8; 0],
                #(#generic_type_fields),*
            }
        });
    }

    /// Record the Api for a type, e.g. enum or struct.
    /// Code generated includes the bindgen entry itself,
    /// various entries for the cxx::bridge to ensure cxx
    /// is aware of the type, and 'use' statements for the final
    /// output mod hierarchy. All are stored in the Api which
    /// this adds.
    fn generate_type(
        &mut self,
        tyname: TypeName,
        type_nature: TypeKind,
        deps: HashSet<TypeName>,
        bindgen_mod_item: Option<Item>,
    ) -> Result<(), ConvertError> {
        let final_ident = make_ident(tyname.get_final_ident());
        let kind_item = match type_nature {
            TypeKind::POD => "Trivial",
            _ => "Opaque",
        };
        let kind_item = make_ident(kind_item);
        let effective_type = self
            .type_database
            .get_effective_type(&tyname)
            .unwrap_or(&tyname);
        if self.type_database.is_on_blocklist(effective_type) {
            return Ok(());
        }
        let tynamestring = effective_type.to_cpp_name();
        let mut for_extern_c_ts = if effective_type.has_namespace() {
            let ns_string = effective_type
                .ns_segment_iter()
                .cloned()
                .collect::<Vec<String>>()
                .join("::");
            quote! {
                #[namespace = #ns_string]
            }
        } else {
            TokenStream2::new()
        };

        let mut fulltypath = Vec::new();
        // We can't use parse_quote! here because it doesn't support type aliases
        // at the moment.
        let colon = TokenTree::Punct(proc_macro2::Punct::new(':', proc_macro2::Spacing::Joint));
        for_extern_c_ts.extend(
            [
                TokenTree::Ident(make_ident("type")),
                TokenTree::Ident(final_ident.clone()),
                TokenTree::Punct(proc_macro2::Punct::new('=', proc_macro2::Spacing::Alone)),
                TokenTree::Ident(make_ident("super")),
                colon.clone(),
                colon.clone(),
                TokenTree::Ident(make_ident("bindgen")),
                colon.clone(),
                colon.clone(),
                TokenTree::Ident(make_ident("root")),
                colon.clone(),
                colon.clone(),
            ]
            .to_vec(),
        );
        fulltypath.push(make_ident("bindgen"));
        fulltypath.push(make_ident("root"));
        for segment in tyname.ns_segment_iter() {
            let id = make_ident(segment);
            for_extern_c_ts
                .extend([TokenTree::Ident(id.clone()), colon.clone(), colon.clone()].to_vec());
            fulltypath.push(id);
        }
        for_extern_c_ts.extend(
            [
                TokenTree::Ident(final_ident.clone()),
                TokenTree::Punct(proc_macro2::Punct::new(';', proc_macro2::Spacing::Alone)),
            ]
            .to_vec(),
        );
        let bridge_item = match type_nature {
            TypeKind::ForwardDeclaration => None,
            _ => Some(Item::Impl(parse_quote! {
                impl UniquePtr<#final_ident> {}
            })),
        };
        fulltypath.push(final_ident.clone());
        let api = Api {
            ns: tyname.get_namespace().clone(),
            id: final_ident.clone(),
            use_stmt: Use::Used,
            global_items: vec![Item::Impl(parse_quote! {
                unsafe impl cxx::ExternType for #(#fulltypath)::* {
                    type Id = cxx::type_id!(#tynamestring);
                    type Kind = cxx::kind::#kind_item;
                }
            })],
            bridge_item,
            extern_c_mod_item: Some(ForeignItem::Verbatim(for_extern_c_ts)),
            additional_cpp: None,
            deps,
            id_for_allowlist: None,
            bindgen_mod_item,
        };
        self.add_api(api);
        self.type_converter.push(tyname);
        Ok(())
    }

    fn build_include_foreign_items(&self, has_additional_cpp_needs: bool) -> Vec<ForeignItem> {
        let extra_inclusion = if has_additional_cpp_needs {
            Some("autocxxgen.h".to_string())
        } else {
            None
        };
        let chained = self.include_list.iter().chain(extra_inclusion.iter());
        chained
            .map(|inc| {
                ForeignItem::Macro(parse_quote! {
                    include!(#inc);
                })
            })
            .collect()
    }

    /// Generate lots of 'use' statements to pull cxxbridge items into the output
    /// mod hierarchy according to C++ namespaces.
    fn generate_final_use_statements(input_items: &[Api]) -> Vec<Item> {
        let mut output_items = Vec::new();
        let ns_entries = NamespaceEntries::new(input_items);
        Self::append_child_use_namespace(&ns_entries, &mut output_items);
        output_items
    }

    fn append_child_use_namespace(ns_entries: &NamespaceEntries, output_items: &mut Vec<Item>) {
        for item in ns_entries.entries() {
            let id = &item.id;
            match &item.use_stmt {
                Use::UsedWithAlias(alias) => output_items.push(Item::Use(parse_quote!(
                    pub use cxxbridge :: #id as #alias;
                ))),
                Use::Used => output_items.push(Item::Use(parse_quote!(
                    pub use cxxbridge :: #id;
                ))),
                Use::Unused => {}
            };
        }
        for (child_name, child_ns_entries) in ns_entries.children() {
            let child_id = make_ident(child_name);
            let mut new_mod: ItemMod = parse_quote!(
                pub mod #child_id {
                    use super::cxxbridge;
                }
            );
            Self::append_child_use_namespace(
                child_ns_entries,
                &mut new_mod.content.as_mut().unwrap().1,
            );
            output_items.push(Item::Mod(new_mod));
        }
    }

    fn append_uses_for_ns(&mut self, items: &mut Vec<Item>, ns: &Namespace) {
        let mut use_stmts = self.use_stmts_by_mod.remove(&ns).unwrap_or_default();
        items.append(&mut use_stmts);
    }

    fn append_child_bindgen_namespace(
        &mut self,
        ns_entries: &NamespaceEntries,
        output_items: &mut Vec<Item>,
        ns: &Namespace,
    ) {
        for item in ns_entries.entries() {
            output_items.extend(item.bindgen_mod_item.iter().cloned());
        }
        for (child_name, child_ns_entries) in ns_entries.children() {
            let new_ns = ns.push((*child_name).clone());
            let child_id = make_ident(child_name);

            let mut inner_output_items = Vec::new();
            self.append_child_bindgen_namespace(child_ns_entries, &mut inner_output_items, &new_ns);
            if !inner_output_items.is_empty() {
                let mut new_mod: ItemMod = parse_quote!(
                    pub mod #child_id {
                    }
                );
                self.append_uses_for_ns(&mut inner_output_items, &new_ns);
                new_mod.content.as_mut().unwrap().1 = inner_output_items;
                output_items.push(Item::Mod(new_mod));
            }
        }
    }

    fn generate_final_bindgen_mods(&mut self, input_items: &[Api]) -> Vec<Item> {
        let mut output_items = Vec::new();
        let ns = Namespace::new();
        let ns_entries = NamespaceEntries::new(input_items);
        self.append_child_bindgen_namespace(&ns_entries, &mut output_items, &ns);
        self.append_uses_for_ns(&mut output_items, &ns);
        output_items
    }
}

impl<'a> ForeignModConversionCallbacks for BridgeConversion<'a> {
    fn convert_boxed_type(
        &self,
        ty: Box<Type>,
        ns: &Namespace,
    ) -> Result<(Box<Type>, HashSet<TypeName>), ConvertError> {
        let annotated = self.type_converter.convert_boxed_type(ty, ns)?;
        Ok((annotated.ty, annotated.types_encountered))
    }

    fn is_pod(&self, ty: &TypeName) -> bool {
        self.byvalue_checker.is_pod(ty)
    }

    fn add_api(&mut self, api: Api) {
        self.apis.push(api);
    }

    fn get_cxx_bridge_name(
        &mut self,
        type_name: Option<&str>,
        found_name: &str,
        ns: &Namespace,
    ) -> String {
        self.bridge_name_tracker
            .get_unique_cxx_bridge_name(type_name, found_name, ns)
    }

    fn ok_to_use_rust_name(&mut self, rust_name: &str) -> bool {
        self.rust_name_tracker.ok_to_use_rust_name(rust_name)
    }

    fn is_on_allowlist(&self, type_name: &TypeName) -> bool {
        self.type_database.is_on_allowlist(type_name)
    }

    fn avoid_generating_type(&self, type_name: &TypeName) -> bool {
        self.type_database.is_on_blocklist(type_name) || self.incomplete_types.contains(type_name)
    }
}
