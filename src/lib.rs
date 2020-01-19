extern crate proc_macro;
use boolean_expression::{Cube, CubeList, CubeVar};
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{
    parse_macro_input, parse_quote,
    spanned::Spanned,
    token::{Brace, Paren},
    visit_mut::{self, VisitMut},
    Attribute, Block, Expr, ExprBlock, ExprLit, ExprMacro, ExprMatch, ExprTuple, Ident, Lit,
    LitInt, LitStr, Local, Pat, PatIdent, PatTuple, Path, Stmt, Token, Type,
};

#[cfg(test)]
mod test;

fn rewrite_let(i: &mut Local) {
    i.attrs.retain(|attr| !path_eq(&attr.path, "bitmatch"));
    let pat = match pat_str(&i.pat) {
        Some(pat) => pat,
        None => panic!("Requires strings as patterns"),
    };
    if !irrefutable_pat(&pat) {
        panic!("Only irrefutable bit-patterns are allowed, found: {}", pat);
    }
    let var_list = vars(&pat);
    i.pat = Pat::Tuple(PatTuple {
        attrs: Vec::new(),
        paren_token: Paren([i.pat.span()]),
        elems: var_list
            .iter()
            .map(|v| {
                Pat::Ident(PatIdent {
                    attrs: Vec::new(),
                    by_ref: None,
                    mutability: None,
                    ident: Ident::new(&format!("{}", v), i.pat.span()),
                    subpat: None,
                })
            })
            .collect(),
    });
    let ident = Ident::new("orig", i.pat.span());
    let (eq, init) = match i.init.clone() {
        Some(i) => i,
        None => panic!("#[bitmatch] let can only be used with an initializer"),
    };
    let result = ExprTuple {
        attrs: Vec::new(),
        paren_token: Paren([init.span()]),
        elems: var_list
            .iter()
            .map(|&v| {
                let mask = mask_for(v, &pat);
                extract_with_mask(&mask, &parse_quote! { #ident })
            })
            .collect(),
    };
    i.init = Some((
        eq,
        parse_quote! {{
            let #ident = #init;
            #result
        }},
    ));
}

fn rewrite_match(i: &mut ExprMatch) {
    i.attrs.retain(|attr| !path_eq(&attr.path, "bitmatch"));
    let mut cases = Vec::with_capacity(i.arms.len());
    let mut cubelist = CubeList::new();
    for arm in &i.arms {
        match pat_str(&arm.pat) {
            Some(s) => {
                if arm.guard.is_none() {
                    cubelist = cubelist.merge(&CubeList::from_list(&[to_cube(&s)]));
                }
                cases.push(s);
            }
            None => {
                let pat = &arm.pat;
                panic!(
                    "#[bitmatch] match patterns must be string literals, but found {}",
                    quote! { #pat }
                )
            }
        }
    }
    if !true_cubelist(&cubelist) {
        panic!("Some cases not covered. Cases: {:?}", cases);
    }
    for (arm, case) in i.arms.iter_mut().zip(&cases) {
        let vars = vars(case);
        let ident = Ident::new("bits", arm.pat.span());
        let (if_, guard) = pattern_guard(&ident, case);
        arm.pat = PatIdent {
            attrs: Vec::new(),
            by_ref: None,
            mutability: None,
            ident: ident.clone(),
            subpat: None,
        }
        .into();
        if let Some((_, old_guard)) = &arm.guard {
            let extra_guard = wrap_with_bindings(&ident, &case, &vars, &*old_guard);
            arm.guard = Some((if_, parse_quote! { #guard && #extra_guard }));
        } else {
            arm.guard = Some((if_, guard));
        }
        if !vars.is_empty() {
            arm.body = Box::new(wrap_with_bindings(&ident, &case, &vars, &arm.body));
        }
    }
    i.arms
        .push(parse_quote! { _ => unreachable!("#[bitmatch] fallback branch") });
}

fn rewrite_macro(i: &mut Expr) {
    let span = i.span();
    let template = if let Expr::Macro(expr) = i {
        match expr.mac.parse_body::<LitStr>() {
            Ok(s) => s.value(),
            _ => panic!("The bitpack!() takes a single string literal"),
        }
    } else {
        unreachable!()
    };
    let vars = vars(&template);
    let mut clauses = Vec::new();
    for &var in &vars {
        let ident = Ident::new(&format!("{}", var), span);
        let mask = mask_for(var, &template);
        clauses.push(insert_with_mask(&mask, &parse_quote!(#ident)));
    }
    *i = or_all(&clauses);
}

struct BitmatchVisitor;
impl VisitMut for BitmatchVisitor {
    fn visit_local_mut(&mut self, i: &mut Local) {
        if has_bitmatch_attr(&i.attrs) {
            rewrite_let(i);
        }
        visit_mut::visit_local_mut(self, i);
    }

    fn visit_expr_match_mut(&mut self, i: &mut ExprMatch) {
        if has_bitmatch_attr(&i.attrs) {
            rewrite_match(i);
        }
        visit_mut::visit_expr_match_mut(self, i);
    }

    fn visit_expr_macro_mut(&mut self, i: &mut ExprMacro) {
        visit_mut::visit_expr_macro_mut(self, i);
    }

    fn visit_expr_mut(&mut self, i: &mut Expr) {
        if let Expr::Macro(mac) = i {
            if path_eq(&mac.mac.path, "bitpack") {
                rewrite_macro(i);
            }
        }
        visit_mut::visit_expr_mut(self, i);
    }
}

fn has_bitmatch_attr(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| path_eq(&attr.path, "bitmatch"))
}

fn path_eq(path: &Path, name: &str) -> bool {
    match path.get_ident() {
        Some(ident) => ident == &Ident::new(name, ident.span()),
        None => false,
    }
}

fn to_cube(lit: &str) -> Cube {
    let lit = lit.trim();
    let len = lit.chars().filter(|&c| c != '_').count();
    let mut cube = Cube::true_cube(len);
    let mut i = 0;
    for c in lit.chars().rev() {
        if c == '_' {
            continue;
        }
        match c {
            '_' => continue,
            '0' => cube = cube.with_var(i, CubeVar::False),
            '1' => cube = cube.with_var(i, CubeVar::True),
            _ => (),
        }
        i += 1;
    }
    cube
}

fn true_cubelist(cubelist: &CubeList) -> bool {
    cubelist.cubes().count() == 1
        && cubelist
            .cubes()
            .next()
            .unwrap()
            .vars()
            .all(|v| v == &CubeVar::DontCare)
}

fn pat_str(p: &Pat) -> Option<String> {
    match p {
        Pat::Lit(pl) => match &*pl.expr {
            Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    }
}

fn pat_mask(p: &str) -> String {
    p.chars()
        .flat_map(|c| match c {
            '0' | '1' => Some('1'),
            '_' => None,
            _ => Some('0'),
        })
        .collect()
}

fn pat_value(p: &str) -> String {
    p.chars()
        .flat_map(|c| match c {
            '0' | '1' => Some(c),
            '_' => None,
            _ => Some('0'),
        })
        .collect()
}

fn make_int_bits(bits: &str) -> Expr {
    let lit = LitInt::new(&format!("0b{}", bits), Span::call_site());
    parse_quote!(#lit)
}

fn pattern_guard(item: &Ident, case: &str) -> (Token![if], Box<Expr>) {
    let mask = make_int_bits(&pat_mask(case));
    let value = make_int_bits(&pat_value(case));
    (
        Token![if]([item.span()]),
        Box::new(parse_quote!(#item & #mask == #value)),
    )
}

fn vars(p: &str) -> Vec<char> {
    let mut items: Vec<_> = p
        .chars()
        .filter(|&c| c != '0' && c != '1' && c != '_' && c != '?')
        .collect();
    items.sort();
    items.dedup();
    items
}

fn mask_for(v: char, p: &str) -> String {
    p.chars()
        .flat_map(|c| match c {
            '_' => None,
            c if c == v => Some('1'),
            _ => Some('0'),
        })
        .collect()
}

fn irrefutable_pat(p: &str) -> bool {
    p.chars().all(|p| p != '0' && p != '1')
}

fn mask_segments(m: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut len = 0;
    for (i, c) in m.chars().rev().enumerate() {
        if len == 0 && c == '1' {
            start = i;
        }
        if c == '1' {
            len += 1;
        }
        if len != 0 && c == '0' {
            result.push((start, len));
            len = 0;
        }
    }
    if len != 0 {
        result.push((start, len));
    }
    result
}

fn int_of_width(m: &str) -> Type {
    if m.len() > 128 {
        panic!("Unupported: bit pattern {:?} wider than 128 bits", m)
    } else if m.len() > 64 {
        parse_quote!(u128)
    } else if m.len() > 32 {
        parse_quote!(u64)
    } else if m.len() > 16 {
        parse_quote!(u32)
    } else if m.len() > 8 {
        parse_quote!(u16)
    } else {
        parse_quote!(u8)
    }
}

fn extract_with_mask(m: &str, expr: &Expr) -> Expr {
    let mut clauses = Vec::new();
    let mut cumulative = 0;
    for (start, count) in mask_segments(m) {
        let amt = start - cumulative;
        let mask = LitInt::new(&format!("0x{:X}", ((1 << count) - 1) << start), expr.span());
        clauses.push(parse_quote!((#expr & #mask) >> #amt));
        cumulative += count;
    }
    or_all(&clauses)
}

fn insert_with_mask(m: &str, expr: &Expr) -> Expr {
    let ty = int_of_width(m);
    let mut clauses = Vec::new();
    let mut cumulative = 0;
    for (start, count) in mask_segments(m) {
        let amt = start - cumulative;
        let mask = LitInt::new(
            &format!("0x{:X}", ((1 << count) - 1) << cumulative),
            expr.span(),
        );
        clauses.push(parse_quote!((#expr as #ty & #mask) << #amt));
        cumulative += count;
    }
    or_all(&clauses)
}

fn or_all(clauses: &[Expr]) -> Expr {
    if clauses.is_empty() {
        parse_quote!(0)
    } else {
        parse_quote!( #(#clauses)|* )
    }
}

fn wrap_with_bindings(ident: &Ident, case: &str, vars: &[char], expr: &Expr) -> Expr {
    let binds = vars.iter().map(|&var| {
        let bind = Ident::new(&format!("{}", var), expr.span());
        let mask = mask_for(var, case);
        let extracted = extract_with_mask(&mask, &parse_quote! { #ident });
        quote! { let #bind = #extracted; }
    });
    parse_quote! {{
        #( #binds )*
        #expr
    }}
}

#[proc_macro_attribute]
pub fn bitmatch(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as syn::Item);
    BitmatchVisitor.visit_item_mut(&mut input);
    TokenStream::from(quote! { #input })
}
