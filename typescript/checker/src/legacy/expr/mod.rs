use super::{
    control_flow::{Comparator, RemoveTypes},
    export::pat_to_ts_fn_param,
    Analyzer,
};
use crate::{
    legacy::{instantiate_class, ValidationResult},
    builtin_types,
    errors::Error,
    ty::{
        self, Alias, Array, CallSignature, Class, ClassInstance, ClassMember, ConstructorSignature,
        EnumVariant, IndexSignature, Interface, Intersection, Method, MethodSignature, Module,
        PropertySignature, Static, Tuple, Type, TypeElement, TypeLit, TypeParamDecl, TypeRef,
        TypeRefExt, Union,
    },
    util::{EqIgnoreNameAndSpan, EqIgnoreSpan, IntoCow},
};
use std::{borrow::Cow, iter::once, sync::Arc};
use swc_atoms::{js_word, JsWord};
use swc_common::{util::iter::IteratorExt as _, Fold, FoldWith, Span, Spanned};
use swc_ecma_ast::*;
use swc_ts_checker_macros::validator;

mod bin;
mod call_new;
mod type_cast;
mod unary;
mod update;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeOfMode {
    /// Used for l-values.
    ///
    /// This is used to allow
    ///
    /// ```ts
    /// type Num = { '0': string } | { [n: number]: number }
    /// declare var num: Num
    /// num[0] = 1
    /// num['0'] = 'ok'
    /// ```
    LValue,
    /// Use for r-values.
    RValue,
}

impl Analyzer<'_, '_> {
    pub fn validate_expr(&mut self, e: &Expr) -> ValidationResult {
        self.validate_expr_with_extra(e, TypeOfMode::RValue, None)
    }

    pub fn validate_expr_with_extra(
        &mut self,
        e: &Expr,
        type_mode: TypeOfMode,
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> ValidationResult {
        let span = e.span();

        match *e {
            Expr::This(ThisExpr { span }) => {
                if let Some(ref ty) = self.scope.this() {
                    return Ok(ty.static_cast());
                }
                return Ok(Cow::Owned(Type::from(TsThisType { span })));
            }

            Expr::Ident(ref i) => self.type_of_ident(i, type_mode),

            Expr::Array(ArrayLit { ref elems, .. }) => {
                let mut types: Vec<TypeRef> = Vec::with_capacity(elems.len());

                for elem in elems {
                    let span = elem.span();
                    match elem {
                        Some(ExprOrSpread {
                            spread: None,
                            ref expr,
                        }) => {
                            let ty = self.type_of(expr)?;
                            types.push(ty)
                        }
                        Some(ExprOrSpread {
                            spread: Some(..), ..
                        }) => unimplemented!("type of array spread"),
                        None => {
                            let ty = Type::undefined(span);
                            types.push(ty.into_cow())
                        }
                    }
                }

                return Ok(Type::Tuple(Tuple { span, types }).into_cow());
            }

            Expr::Lit(Lit::Bool(v)) => {
                return Ok(Type::Lit(TsLitType {
                    span: v.span,
                    lit: TsLit::Bool(v),
                })
                .into_cow());
            }
            Expr::Lit(Lit::Str(ref v)) => {
                return Ok(Type::Lit(TsLitType {
                    span: v.span,
                    lit: TsLit::Str(v.clone()),
                })
                .into_cow());
            }
            Expr::Lit(Lit::Num(v)) => {
                return Ok(Type::Lit(TsLitType {
                    span: v.span,
                    lit: TsLit::Number(v),
                })
                .into_cow());
            }
            Expr::Lit(Lit::Null(Null { span })) => {
                return Ok(Type::Keyword(TsKeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsNullKeyword,
                })
                .into_cow());
            }
            Expr::Lit(Lit::Regex(..)) => {
                return Ok(TsType::TsTypeRef(TsTypeRef {
                    span,
                    type_name: TsEntityName::Ident(Ident {
                        span,
                        sym: js_word!("RegExp"),
                        optional: false,
                        type_ann: None,
                    }),
                    type_params: None,
                })
                .into_cow());
            }

            Expr::Paren(ParenExpr { ref expr, .. }) => return self.type_of(expr),

            Expr::Tpl(ref t) => {
                // Check if tpl is constant. If it is, it's type is string literal.
                if t.exprs.is_empty() {
                    return Ok(Type::Lit(TsLitType {
                        span: t.span(),
                        lit: TsLit::Str(
                            t.quasis[0]
                                .cooked
                                .clone()
                                .unwrap_or_else(|| t.quasis[0].raw.clone()),
                        ),
                    })
                    .into_cow());
                }

                return Ok(Type::Keyword(TsKeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsStringKeyword,
                })
                .into_cow());
            }

            Expr::Bin(ref expr) => self.type_of_bin_expr(&expr),

            Expr::Unary(UnaryExpr {
                span, op, ref arg, ..
            }) => {
                match op {
                    op!("typeof") => {
                        return Ok(Type::Keyword(TsKeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsStringKeyword,
                        })
                        .into_cow());
                    }

                    // `delete foo` returns bool
                    op!("delete") => {
                        return Ok(Type::Keyword(TsKeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsBooleanKeyword,
                        })
                        .owned());
                    }

                    op!("void") => return Ok(Type::undefined(span).owned()),

                    _ => {}
                }

                let arg_ty = self.type_of(arg)?;
                match *arg_ty {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsUnknownKeyword,
                        ..
                    }) => return Err(Error::Unknown { span: arg.span() }),
                    _ => {}
                }

                match op {
                    op!("!") => return Ok(negate(self.type_of(arg)?.into_owned()).into_cow()),

                    op!(unary, "-") | op!(unary, "+") => {
                        return Ok(Type::Keyword(TsKeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                        })
                        .owned());
                    }

                    op!("~") => {
                        return Ok(Type::Keyword(TsKeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                        })
                        .owned());
                    }

                    op!("typeof") | op!("delete") | op!("void") => unreachable!(),
                }
            }

            Expr::TsAs(TsAsExpr { ref type_ann, .. }) => {
                return Ok(instantiate_class(type_ann.clone().into_cow()));
            }
            Expr::TsTypeCast(TsTypeCastExpr { ref type_ann, .. }) => {
                return Ok(instantiate_class(type_ann.type_ann.clone().into_cow()));
            }

            Expr::TsNonNull(TsNonNullExpr { ref expr, .. }) => {
                return self.type_of(expr).map(|ty| {
                    // TODO: Optimize

                    ty.remove_falsy()
                });
            }

            Expr::Object(ObjectLit { span, ref props }) => {
                let mut members = Vec::with_capacity(props.len());
                let mut special_type = None;

                for prop in props.iter() {
                    match *prop {
                        PropOrSpread::Prop(ref prop) => {
                            members.push(self.type_of_prop(&prop)?);
                        }
                        PropOrSpread::Spread(SpreadElement { ref expr, .. }) => {
                            match self.type_of(&expr)?.into_owned() {
                                Type::TypeLit(TypeLit {
                                    members: spread_members,
                                    ..
                                }) => {
                                    members.extend(spread_members);
                                }

                                // Use last type on ...any or ...unknown
                                ty
                                @
                                Type::Keyword(TsKeywordType {
                                    kind: TsKeywordTypeKind::TsUnknownKeyword,
                                    ..
                                })
                                | ty
                                @
                                Type::Keyword(TsKeywordType {
                                    kind: TsKeywordTypeKind::TsAnyKeyword,
                                    ..
                                }) => special_type = Some(ty),

                                ty => unimplemented!("spread with non-type-lit: {:#?}", ty),
                            }
                        }
                    }
                }

                if let Some(ty) = special_type {
                    return Ok(ty.into_cow());
                }

                return Ok(Type::TypeLit(TypeLit { span, members }).into_cow());
            }

            // https://github.com/Microsoft/TypeScript/issues/26959
            Expr::Yield(..) => return Ok(Type::any(span).into_cow()),

            Expr::Update(..) => {
                return Ok(Type::Keyword(TsKeywordType {
                    kind: TsKeywordTypeKind::TsNumberKeyword,
                    span,
                })
                .into_cow());
            }

            Expr::Cond(CondExpr {
                ref test,
                ref cons,
                ref alt,
                ..
            }) => {
                match **test {
                    Expr::Ident(ref i) => {
                        // Check `declaring` before checking variables.
                        if self.declaring.contains(&i.sym) {
                            if self.allow_ref_declaring {
                                return Ok(Type::any(span).owned());
                            } else {
                                return Err(Error::ReferencedInInit { span });
                            }
                        }
                    }
                    _ => {}
                }

                let cons_ty = self.type_of(cons)?.into_owned();
                let alt_ty = self.type_of(alt)?.into_owned();

                return Ok(Type::union(once(cons_ty).chain(once(alt_ty))).into_cow());
            }

            Expr::New(NewExpr {
                ref callee,
                ref type_args,
                ref args,
                ..
            }) => {
                let callee_type = self.extract_call_new_expr_member(
                    callee,
                    ExtractKind::New,
                    args.as_ref().map(|v| &**v).unwrap_or_else(|| &[]),
                    type_args.as_ref(),
                )?;
                return Ok(callee_type);
            }

            Expr::Call(CallExpr {
                callee: ExprOrSuper::Expr(ref callee),
                ref args,
                ref type_args,
                ..
            }) => {
                let ret_ty = self
                    .extract_call_new_expr_member(
                        callee,
                        ExtractKind::Call,
                        args,
                        type_args.as_ref(),
                    )
                    .map(|v| v)?;

                return Ok(ret_ty);
            }

            // super() returns any
            Expr::Call(CallExpr {
                callee: ExprOrSuper::Super(..),
                ..
            }) => return Ok(Type::any(span).into_cow()),

            Expr::Seq(SeqExpr { ref exprs, .. }) => {
                assert!(exprs.len() >= 1);

                let mut is_any = false;
                for e in exprs.iter() {
                    match **e {
                        Expr::Ident(ref i) => {
                            if self.declaring.contains(&i.sym) {
                                is_any = true;
                            }
                        }
                        _ => {}
                    }
                    match self.type_of(e) {
                        Ok(..) => {}
                        Err(Error::ReferencedInInit { .. }) => {
                            is_any = true;
                        }
                        Err(..) => {}
                    }
                }
                if is_any {
                    return Ok(Type::any(span).owned());
                }

                return self.type_of(&exprs.last().unwrap());
            }

            Expr::Await(AwaitExpr { .. }) => unimplemented!("typeof(AwaitExpr)"),

            Expr::Class(ClassExpr { ref class, .. }) => {
                return Ok(self.type_of_class(None, class)?.owned());
            }

            Expr::Arrow(ref e) => return Ok(self.type_of_arrow_fn(e)?.owned()),

            Expr::Fn(FnExpr { ref function, .. }) => {
                return Ok(self.type_of_fn(&function)?.owned());
            }

            Expr::Member(ref expr) => {
                return self.type_of_member_expr(expr, type_mode);
            }

            Expr::MetaProp(..) => unimplemented!("typeof(MetaProp)"),

            Expr::Assign(AssignExpr {
                left: PatOrExpr::Pat(box Pat::Ident(ref i)),
                ref right,
                ..
            }) => {
                if self.declaring.contains(&i.sym) {
                    return Ok(Type::any(span).owned());
                }

                return self.type_of(right);
            }

            Expr::Assign(AssignExpr {
                left: PatOrExpr::Expr(ref left),
                ref right,
                ..
            }) => {
                match **left {
                    Expr::Ident(ref i) => {
                        if self.declaring.contains(&i.sym) {
                            return Ok(Type::any(span).owned());
                        }
                    }
                    _ => {}
                }

                return self.type_of(right);
            }

            Expr::Assign(AssignExpr { ref right, .. }) => return self.type_of(right),

            Expr::TsTypeAssertion(TsTypeAssertion { ref type_ann, .. }) => {
                return Ok(Type::from(type_ann.clone()).owned());
            }

            Expr::Invalid(ref i) => return Ok(Type::any(i.span()).owned()),

            _ => unimplemented!("typeof ({:#?})", e),
        }
    }
}

impl Analyzer<'_, '_> {
    pub(super) fn type_of_ident(&self, i: &Ident, type_mode: TypeOfMode) -> Result<TypeRef, Error> {
        let span = i.span();

        match i.sym {
            js_word!("arguments") => return Ok(Type::any(span).owned()),
            js_word!("Symbol") => {
                return Ok(builtin_types::get_var(self.libs, i.span, &js_word!("Symbol"))?.owned());
            }
            js_word!("undefined") => return Ok(Type::undefined(span).into_cow()),
            js_word!("void") => return Ok(Type::any(span).into_cow()),
            js_word!("eval") => match type_mode {
                TypeOfMode::LValue => return Err(Error::CannotAssignToNonVariable { span }),
                TypeOfMode::RValue => {
                    return Ok(Type::Function(ty::Function {
                        span,
                        params: vec![],
                        ret_ty: box Type::any(span).owned(),
                        type_params: None,
                    })
                    .owned());
                }
            },
            _ => {}
        }

        if i.sym == js_word!("require") {
            unreachable!("typeof(require('...'))");
        }

        if let Some(ty) = self.resolved_imports.get(&i.sym) {
            println!(
                "({}) type_of({}): resolved import",
                self.scope.depth(),
                i.sym
            );
            return Ok(ty.static_cast());
        }

        if let Some(ty) = self.find_type(&i.sym) {
            println!("({}) type_of({}): find_type", self.scope.depth(), i.sym);
            return Ok(ty.clone().respan(span).owned());
        }

        // Check `declaring` before checking variables.
        if self.declaring.contains(&i.sym) {
            println!(
                "({}) reference in initialization: {}",
                self.scope.depth(),
                i.sym
            );

            if self.allow_ref_declaring {
                return Ok(Type::any(span).owned());
            } else {
                return Err(Error::ReferencedInInit { span });
            }
        }

        if let Some(ty) = self.find_var_type(&i.sym) {
            println!("({}) type_of({}): find_var_type", self.scope.depth(), i.sym);
            return Ok(ty.clone().respan(span).owned());
        }

        if let Some(_var) = self.find_var(&i.sym) {
            // TODO: Infer type or use type hint to handle
            //
            // let id: (x: Foo) => Foo = x => x;
            //
            return Ok(Type::any(span).into_cow());
        }

        if let Ok(ty) = builtin_types::get_var(self.libs, span, &i.sym) {
            return Ok(ty.owned());
        }

        println!(
            "({}) type_of(): undefined symbol: {}",
            self.scope.depth(),
            i.sym,
        );

        return Err(Error::UndefinedSymbol { span: i.span });
    }

    fn type_of_member_expr(
        &self,
        expr: &MemberExpr,
        type_mode: TypeOfMode,
    ) -> Result<TypeRef, Error> {
        let MemberExpr {
            ref obj,
            computed,
            ref prop,
            span,
            ..
        } = *expr;
        //        debug_assert_ne!(span, obj.span());
        //        debug_assert_ne!(span, prop.span());

        let mut errors = vec![];
        match *obj {
            ExprOrSuper::Expr(ref obj) => {
                let obj_ty = match self.type_of(obj) {
                    Ok(ty) => ty,
                    Err(err) => {
                        // Recover error if possible.
                        if computed {
                            errors.push(err);
                            Type::any(span).owned()
                        } else {
                            return Err(err);
                        }
                    }
                };

                if computed {
                    let ty = match self.access_property(span, obj_ty, prop, computed, type_mode) {
                        Ok(v) => Ok(v),
                        Err(err) => {
                            errors.push(err);

                            Err(())
                        }
                    };
                    if errors.is_empty() {
                        return Ok(ty.unwrap());
                    } else {
                        match self.type_of(&prop) {
                            Ok(..) => match ty {
                                Ok(ty) => {
                                    if errors.is_empty() {
                                        return Ok(ty);
                                    } else {
                                        return Err(Error::Errors { span, errors });
                                    }
                                }
                                Err(()) => return Err(Error::Errors { span, errors }),
                            },
                            Err(err) => errors.push(err),
                        }
                    }
                } else {
                    match self.access_property(span, obj_ty, prop, computed, type_mode) {
                        Ok(v) => return Ok(v),
                        Err(err) => {
                            errors.push(err);
                            return Err(Error::Errors { span, errors });
                        }
                    }
                }
            }
            _ => unimplemented!("type_of_member_expr(super.foo)"),
        }

        if errors.len() == 1 {
            return Err(errors.remove(0));
        }
        return Err(Error::Errors { span, errors });
    }

    fn type_of_prop(&self, prop: &Prop) -> Result<TypeElement, Error> {
        let span = prop.span();

        Ok(match *prop {
            Prop::Shorthand(..) => PropertySignature {
                span: prop.span(),
                key: prop_key_to_expr(&prop),
                params: Default::default(),
                optional: false,
                readonly: false,
                computed: false,
                type_ann: Default::default(),
                type_params: Default::default(),
            }
            .into(),

            Prop::KeyValue(ref kv) => {
                let ty = self.type_of(&kv.value)?;
                let ty = self.expand_type(prop.span(), ty)?;

                PropertySignature {
                    span: prop.span(),
                    key: prop_key_to_expr(&prop),
                    params: Default::default(),
                    optional: false,
                    readonly: false,
                    computed: false,
                    type_ann: Some(ty),
                    type_params: Default::default(),
                }
                .into()
            }

            Prop::Assign(ref p) => unimplemented!("type_of_prop(AssignProperty): {:?}", p),
            Prop::Getter(ref p) => PropertySignature {
                span: prop.span(),
                key: prop_key_to_expr(&prop),
                params: Default::default(),
                optional: false,
                readonly: false,
                computed: false,
                type_ann: match self.infer_return_type(p.span()) {
                    Some(ty) => Some(ty.owned()),
                    // This is error, but it's handled by GetterProp visitor.
                    None => None,
                },
                type_params: Default::default(),
            }
            .into(),
            Prop::Setter(ref p) => unimplemented!("type_of_prop(SetterProperty): {:?}", p),

            Prop::Method(ref p) => MethodSignature {
                span,
                readonly: false,
                key: prop_key_to_expr(&prop),
                computed: false,
                optional: false,
                params: p
                    .function
                    .params
                    .iter()
                    .cloned()
                    .map(pat_to_ts_fn_param)
                    .collect(),
                ret_ty: p.function.return_type.clone().map(|v| v.into_cow()),
                type_params: p.function.type_params.clone().map(|v| v.into()),
            }
            .into(),
        })
    }

    fn access_property<'a>(
        &self,
        span: Span,
        obj: TypeRef,
        prop: &Expr,
        computed: bool,
        type_mode: TypeOfMode,
    ) -> Result<TypeRef<'a>, Error> {
        macro_rules! handle_type_els {
            ($members:expr) => {{
                let prop_ty = if computed {
                    self.type_of(prop)?.generalize_lit()
                } else {
                    match prop {
                        Expr::Ident(..) => Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsStringKeyword,
                            span,
                        })
                        .owned(),

                        _ => unreachable!(),
                    }
                };

                for el in $members.iter() {
                    match el {
                        TypeElement::Index(IndexSignature {
                            ref params,
                            ref type_ann,
                            ..
                        }) => {
                            if params.len() != 1 {
                                unimplemented!("Index signature with multiple parameters")
                            }
                            match params[0] {
                                TsFnParam::Ident(ref i) => {
                                    assert!(i.type_ann.is_some());

                                    let index_ty = Type::from(i.type_ann.as_ref().unwrap().clone());
                                    if index_ty.eq_ignore_name_and_span(&*prop_ty) {
                                        if let Some(ref type_ann) = type_ann {
                                            return Ok(type_ann.to_static().owned());
                                        }
                                        return Ok(Type::any(span).owned());
                                    }
                                }

                                _ => unimplemented!("TsFnParam other than index in IndexSignature"),
                            }
                        }
                        _ => {}
                    }

                    if let Some(key) = el.key() {
                        let is_el_computed = match *el {
                            TypeElement::Property(ref p) => p.computed,
                            _ => false,
                        };
                        let is_eq = is_el_computed == computed
                            && match prop {
                                Expr::Ident(Ident { sym: ref value, .. })
                                | Expr::Lit(Lit::Str(Str { ref value, .. })) => match key {
                                    Expr::Ident(Ident {
                                        sym: ref r_value, ..
                                    })
                                    | Expr::Lit(Lit::Str(Str {
                                        value: ref r_value, ..
                                    })) => value == r_value,
                                    _ => false,
                                },
                                _ => false,
                            };
                        if is_eq || key.eq_ignore_span(prop) {
                            match el {
                                TypeElement::Property(ref p) => {
                                    if type_mode == TypeOfMode::LValue && p.readonly {
                                        return Err(Error::ReadOnly { span });
                                    }

                                    if let Some(ref type_ann) = p.type_ann {
                                        return Ok(type_ann.to_static().owned());
                                    }

                                    // TODO: no implicit any?
                                    return Ok(Type::any(span).owned());
                                }

                                _ => {}
                            }
                        }
                    }
                }
            }};
        }

        let obj = obj.generalize_lit();

        // TODO: Remove to_static()
        let obj = self.expand_type(obj.span(), obj.to_static().owned())?;
        match *obj.normalize() {
            Type::Lit(..) => unreachable!(),

            Type::Enum(ref e) => {
                // TODO: Check if variant exists.
                macro_rules! ret {
                    ($sym:expr) => {{
                        // Computed values are not permitted in an enum with string valued members.
                        if e.is_const && type_mode == TypeOfMode::RValue {
                            for m in &e.members {
                                match m.id {
                                    TsEnumMemberId::Ident(Ident { ref sym, .. })
                                    | TsEnumMemberId::Str(Str { value: ref sym, .. }) => {
                                        if sym == $sym {
                                            return Ok(Cow::Owned(Type::Lit(TsLitType {
                                                span: m.span(),
                                                lit: m.val.clone().into(),
                                            })));
                                        }
                                    }
                                }
                            }
                        }

                        if e.is_const && computed {
                            return Err(Error::ConstEnumNonIndexAccess { span: prop.span() });
                        }

                        if e.is_const && type_mode == TypeOfMode::LValue {
                            return Err(Error::InvalidLValue { span: prop.span() });
                        }

                        debug_assert_ne!(span, prop.span());
                        return Ok(Cow::Owned(Type::EnumVariant(EnumVariant {
                            span: match type_mode {
                                TypeOfMode::LValue => prop.span(),
                                TypeOfMode::RValue => span,
                            },
                            enum_name: e.id.sym.clone(),
                            name: $sym.clone(),
                        })));
                    }};
                }
                match *prop {
                    Expr::Ident(Ident { ref sym, .. }) if !computed => {
                        ret!(sym);
                    }
                    Expr::Lit(Lit::Str(Str { value: ref sym, .. })) => {
                        ret!(sym);
                    }
                    Expr::Lit(Lit::Num(Number { value, .. })) => {
                        let idx = value.round() as usize;
                        if e.members.len() > idx {
                            return Ok(Type::Lit(TsLitType {
                                span,
                                lit: e.members[idx].val.clone(),
                            })
                            .owned());
                        }
                        return Ok(Type::Keyword(TsKeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsStringKeyword,
                        })
                        .owned());
                    }

                    _ => {
                        if e.is_const {
                            return Err(Error::ConstEnumNonIndexAccess { span: prop.span() });
                        }
                        return Err(Error::Unimplemented {
                            span,
                            msg: format!("access_property\nProp: {:?}", prop),
                        });
                    }
                }
            }

            // enum Foo { A }
            //
            // Foo.A.toString()
            Type::EnumVariant(EnumVariant {
                ref enum_name,
                ref name,
                span,
                ..
            }) => match self.find_type(enum_name) {
                Some(ref v) => match **v {
                    Type::Enum(ref e) => {
                        for v in e.members.iter() {
                            let new_obj_ty = Cow::Owned(Type::Lit(TsLitType {
                                span,
                                lit: v.val.clone(),
                            }));
                            return self
                                .access_property(span, new_obj_ty, prop, computed, type_mode);
                        }
                        unreachable!("Enum {} does not have a variant named {}", enum_name, name);
                    }
                    _ => unreachable!("Enum named {} does not exist", enum_name),
                },
                _ => unreachable!("Enum named {} does not exist", enum_name),
            },

            Type::Class(ref c) => {
                for v in c.body.iter() {
                    match v {
                        ClassMember::ClassProp(ref class_prop) => {
                            match *class_prop.key {
                                Expr::Ident(ref i) => {
                                    if self.scope.declaring_prop.as_ref() == Some(&i.sym) {
                                        return Err(Error::ReferencedInInit { span });
                                    }
                                }
                                _ => {}
                            }
                            //
                            if (*class_prop.key).eq_ignore_span(&*prop) {
                                return Ok(
                                    match class_prop.type_ann.clone().map(|v| v.into_cow()) {
                                        Some(ty) => ty,
                                        None => Type::any(span).owned(),
                                    },
                                );
                            }
                        }
                        ClassMember::Method(ref mtd) => {
                            let mtd_key = prop_name_to_expr(&mtd.key);
                            if (*mtd_key).eq_ignore_span(&mtd_key) {
                                return Ok(Type::Method(mtd.clone().into_static()).owned());
                            }
                        }
                        _ => unimplemented!("Non-property class member"),
                    }
                }
            }

            Type::Keyword(TsKeywordType {
                kind: TsKeywordTypeKind::TsAnyKeyword,
                ..
            }) => {
                return Ok(Type::Keyword(TsKeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsAnyKeyword,
                })
                .owned());
            }

            Type::Keyword(TsKeywordType {
                kind: TsKeywordTypeKind::TsUnknownKeyword,
                ..
            }) => return Err(Error::Unknown { span: obj.span() }),

            Type::Keyword(TsKeywordType { kind, .. }) => {
                let word = match kind {
                    TsKeywordTypeKind::TsStringKeyword => js_word!("String"),
                    TsKeywordTypeKind::TsNumberKeyword => js_word!("Number"),
                    TsKeywordTypeKind::TsBooleanKeyword => js_word!("Boolean"),
                    TsKeywordTypeKind::TsObjectKeyword => js_word!("Object"),
                    TsKeywordTypeKind::TsSymbolKeyword => js_word!("Symbol"),
                    _ => unimplemented!("access_property: obj: TSKeywordType {:?}", kind),
                };
                let interface = builtin_types::get_type(self.libs, span, &word)?;
                return self.access_property(span, interface.into_cow(), prop, computed, type_mode);
            }

            Type::Array(Array { ref elem_type, .. }) => {
                let array_ty = builtin_types::get_type(self.libs, span, &js_word!("Array"))
                    .expect("Array should be loaded")
                    .owned();

                match self.type_of(prop) {
                    Ok(ty) => match ty.normalize() {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                            ..
                        })
                        | Type::Lit(TsLitType {
                            lit: TsLit::Number(..),
                            ..
                        }) => return Ok(elem_type.to_static().owned()),

                        _ => {}
                    },
                    _ => {}
                }

                return self.access_property(span, array_ty, prop, computed, type_mode);
            }

            Type::Interface(Interface { ref body, .. }) => {
                handle_type_els!(body);

                // TODO: Check parent interfaces

                return Err(Error::NoSuchProperty {
                    span,
                    prop: Some(prop.clone()),
                });
            }

            Type::TypeLit(TypeLit { ref members, .. }) => {
                handle_type_els!(members);

                return Err(Error::NoSuchProperty {
                    span,
                    prop: Some(prop.clone()),
                });
            }

            Type::Union(Union { ref types, .. }) => {
                debug_assert!(types.len() >= 1);

                let mut tys = vec![];
                let mut errors = Vec::with_capacity(types.len());

                for ty in types {
                    match self.access_property(span, Cow::Borrowed(&ty), prop, computed, type_mode)
                    {
                        Ok(ty) => tys.push(ty.into_owned()),
                        Err(err) => errors.push(err),
                    }
                }

                if type_mode != TypeOfMode::LValue {
                    if !errors.is_empty() {
                        return Err(Error::UnionError { span, errors });
                    }
                } else {
                    // In l-value context, it's success if one of types matches it.
                    let is_err = errors.iter().any(|err| match *err {
                        Error::ReadOnly { .. } => true,
                        _ => false,
                    });
                    if tys.is_empty() || is_err {
                        assert_ne!(errors.len(), 0);
                        return Err(Error::UnionError { span, errors });
                    }
                }

                // TODO: Validate that the ty has same type instead of returning union.
                return Ok(Type::union(tys).owned());
            }

            Type::Tuple(Tuple { ref types, .. }) => match *prop {
                Expr::Lit(Lit::Num(n)) => {
                    let v = n.value.round() as i64;
                    if v < 0 || types.len() <= v as usize {
                        return Err(Error::TupleIndexError {
                            span: n.span(),
                            index: v,
                            len: types.len() as u64,
                        });
                    }

                    return Ok(types[v as usize].to_static().owned());
                }
                _ => {
                    if types.is_empty() {
                        return Ok(Type::any(span).owned());
                    }

                    //                    if types.len() == 1 {
                    //                        return Ok(Cow::Borrowed(&types[0]));
                    //                    }

                    return Ok(Type::Union(Union {
                        span,
                        types: types
                            .into_iter()
                            .map(|v| Cow::Owned(v.to_static()))
                            .collect(),
                    })
                    .owned());
                }
            },

            Type::ClassInstance(ClassInstance { ref cls, .. }) => {
                //
                for m in &cls.body {
                    //
                    match *m {
                        ClassMember::ClassProp(ref p) => {
                            // TODO: normalized string / ident
                            if (&*p.key).eq_ignore_name_and_span(&prop) {
                                if let Some(ref ty) = p.type_ann {
                                    return Ok(Type::from(ty.clone()).owned());
                                }

                                return Ok(Type::any(p.key.span()).owned());
                            }
                        }
                        _ => {}
                    }
                }
            }

            Type::Module(Module { ref exports, .. }) => match prop {
                Expr::Ident(Ident { ref sym, .. }) => {
                    if let Some(item) = exports.types.get(sym) {
                        return Ok(Type::Arc(Arc::clone(item)).owned());
                    }
                }
                _ => {}
            },

            Type::This(..) => {
                if let Some(ref this) = self.scope.this {
                    return self.access_property(
                        span,
                        this.clone().owned(),
                        prop,
                        computed,
                        type_mode,
                    );
                }
            }

            _ => {}
        }

        unimplemented!(
            "access_property(MemberExpr):\nObject: {:?}\nProp: {:?}",
            obj,
            prop
        );
    }

    /// In almost case, this method returns `Ok`.
    pub(super) fn validate_type_of_class(
        &mut self,
        name: Option<JsWord>,
        c: &swc_ecma_ast::Class,
    ) -> Result<Type<'static>, Error> {
        for m in c.body.iter() {
            match *m {
                swc_ecma_ast::ClassMember::ClassProp(ref prop) => match prop.type_ann {
                    Some(ref ty) => {
                        let ty = Type::from(ty.clone());
                        if ty.is_any() || ty.is_unknown() {
                        } else {
                            if prop.value.is_none() {
                                // TODO: Uncomment this after implementing a
                                // constructor checker.
                                // self.info
                                //     .errors
                                //     .push(Error::ClassPropertyInitRequired {
                                // span })
                            }
                        }
                    }
                    None => {}
                },
                _ => {}
            }
        }

        self.type_of_class(name, c)
    }

    fn type_of_class(
        &self,
        name: Option<JsWord>,
        c: &swc_ecma_ast::Class,
    ) -> Result<Type<'static>, Error> {
        // let mut type_props = vec![];
        // for member in &c.body {
        //     let span = member.span();
        //     let any = any(span);

        //     match member {
        //         ClassMember::ClassProp(ref p) => {
        //             let ty = match p.type_ann.as_ref().map(|ty|
        // Type::from(&*ty.type_ann)) {                 Some(ty) => ty,
        //                 None => match p.value {
        //                     Some(ref e) => self.type_of(&e)?,
        //                     None => any,
        //                 },
        //             };

        //
        // type_props.push(TypeElement::TsPropertySignature(TsPropertySignature {
        //                 span,
        //                 key: p.key.clone(),
        //                 optional: p.is_optional,
        //                 readonly: p.readonly,
        //                 init: p.value.clone(),
        //                 type_ann: Some(TsTypeAnn {
        //                     span: ty.span(),
        //                     type_ann: box ty.into_owned(),
        //                 }),

        //                 // TODO:
        //                 computed: false,

        //                 // TODO:
        //                 params: Default::default(),

        //                 // TODO:
        //                 type_params: Default::default(),
        //             }));
        //         }

        //         // TODO:
        //         ClassMember::Constructor(ref c) => {
        //             type_props.push(TypeElement::TsConstructSignatureDecl(
        //                 TsConstructSignatureDecl {
        //                     span,

        //                     // TODO:
        //                     type_ann: None,

        //                     params: c
        //                         .params
        //                         .iter()
        //                         .map(|param| match *param {
        //                             PatOrTsParamProp::Pat(ref pat) => {
        //                                 pat_to_ts_fn_param(pat.clone())
        //                             }
        //                             PatOrTsParamProp::TsParamProp(ref prop) => match
        // prop.param {
        // TsParamPropParam::Ident(ref i) => {
        // TsFnParam::Ident(i.clone())                                 }
        //                                 TsParamPropParam::Assign(AssignPat {
        //                                     ref left, ..
        //                                 }) => pat_to_ts_fn_param(*left.clone()),
        //                             },
        //                         })
        //                         .collect(),

        //                     // TODO:
        //                     type_params: Default::default(),
        //                 },
        //             ));
        //         }

        //         // TODO:
        //         ClassMember::Method(..) => {}

        //         // TODO:
        //         ClassMember::TsIndexSignature(..) => {}

        //         ClassMember::PrivateMethod(..) | ClassMember::PrivateProp(..) => {}
        //     }
        // }

        let super_class = match c.super_class {
            Some(ref expr) => Some(box self.type_of(&expr)?.to_static().into_cow()),
            None => None,
        };

        // TODO: Check for implements

        Ok(Type::Class(Class {
            span: c.span,
            name,
            is_abstract: c.is_abstract,
            super_class,
            type_params: c.clone().type_params.map(From::from),
            body: c
                .body
                .clone()
                .into_iter()
                .filter_map(|v| {
                    //
                    Some(match v {
                        swc_ecma_ast::ClassMember::Constructor(v) => {
                            ClassMember::Constructor(v.into())
                        }
                        swc_ecma_ast::ClassMember::Method(v) => {
                            //
                            let ret_ty = match v.function.return_type {
                                Some(v) => box Type::from(v.type_ann).owned(),
                                None => box self
                                    .infer_return_type(v.function.span)
                                    .unwrap_or_else(|| Type::any(v.function.span))
                                    .owned(),
                            };

                            ClassMember::Method(Method {
                                span: v.span,
                                key: v.key,
                                is_static: v.is_static,
                                type_params: v.function.type_params.map(From::from),
                                params: v.function.params,
                                ret_ty,
                            })
                        }
                        swc_ecma_ast::ClassMember::PrivateMethod(_) => return None,
                        swc_ecma_ast::ClassMember::ClassProp(v) => ClassMember::ClassProp(v),
                        swc_ecma_ast::ClassMember::PrivateProp(_) => return None,
                        swc_ecma_ast::ClassMember::TsIndexSignature(v) => {
                            ClassMember::TsIndexSignature(v)
                        }
                    })
                })
                .collect(),
        }))
    }

    pub(super) fn infer_return_type(&self, key: Span) -> Option<Type<'static>> {
        debug_assert!(
            self.inferred_return_types.borrow_mut().get(&key).is_some(),
            "infer_return_type: key={:?}, entries={:?}",
            key,
            self.inferred_return_types
        );

        // TODO: remove entry (currently empty vertor is inserted)
        let types = ::std::mem::replace(
            &mut *self
                .inferred_return_types
                .borrow_mut()
                .get_mut(&key)
                .unwrap(),
            Default::default(),
        );

        // TODO: Handle recursive function.

        match types.len() {
            0 => None,
            _ => Some(Type::union(types)),
        }
    }

    pub(super) fn type_of_arrow_fn(&self, f: &ArrowExpr) -> Result<Type<'static>, Error> {
        let declared_ret_ty = f.return_type.as_ref().map(|ret_ty| {
            self.expand_type(f.span, Cow::Owned(Type::from(ret_ty.type_ann.clone())))
                .map(|v| v.to_static())
        });
        let declared_ret_ty = match declared_ret_ty {
            Some(Ok(ty)) => {
                let span = ty.span();
                Some(match ty {
                    Type::Class(cls) => Type::ClassInstance(ClassInstance {
                        span,
                        cls,
                        type_args: None,
                    }),
                    _ => ty,
                })
            }
            Some(Err(err)) => return Err(err),
            None => None,
        };

        let inferred_return_type = self.infer_return_type(f.span());
        if let Some(ref declared) = declared_ret_ty {
            let span = inferred_return_type.span();
            if let Some(ref inferred) = inferred_return_type {
                self.assign(declared, inferred, span)?;
            }
        }

        Ok(ty::Function {
            span: f.span,
            params: f.params.iter().cloned().map(pat_to_ts_fn_param).collect(),
            type_params: f.type_params.clone().map(From::from),
            ret_ty: box declared_ret_ty
                .unwrap_or_else(|| {
                    inferred_return_type
                        .map(|v| v.to_static())
                        .unwrap_or_else(|| Type::any(f.span))
                })
                .owned(),
        }
        .into())
    }

    pub(super) fn type_of_fn(&self, f: &Function) -> Result<Type<'static>, Error> {
        let (ty, err) = self.type_of_fn_detail(f)?;
        match err {
            Error::Errors { ref errors, .. } => {
                if errors.is_empty() {
                    return Ok(ty);
                } else {
                    return Err(err);
                }
            }
            _ => unreachable!(),
        }
    }

    /// Returned [Ok] contains type information with recoverable errors.
    pub(super) fn type_of_fn_detail(&self, f: &Function) -> Result<(Type<'static>, Error), Error> {
        let mut errors = vec![];

        let declared_ret_ty = f
            .return_type
            .as_ref()
            .map(|ret_ty| Type::from(ret_ty.type_ann.clone()))
            .map(Cow::Owned);

        let declared_ret_ty = match declared_ret_ty.clone().map(|ret_ty| {
            self.expand_type(f.span, ret_ty).map(|ty| {
                let span = ty.span();
                match ty.to_static() {
                    Type::Class(cls) => Type::ClassInstance(ClassInstance {
                        span,
                        cls,
                        type_args: None,
                    }),
                    ty => ty,
                }
            })
        }) {
            Some(Ok(ty)) => Some(ty),
            Some(Err(err)) => {
                errors.push(err);
                Some(declared_ret_ty.unwrap().to_static())
            }
            None => None,
        };

        let inferred_return_type = f.body.as_ref().map(|_| self.infer_return_type(f.span));
        let inferred_return_type = match inferred_return_type {
            Some(Some(inferred_return_type)) => {
                if let Some(ref declared) = declared_ret_ty {
                    let span = inferred_return_type.span();

                    self.assign(&declared, &inferred_return_type, span)?;
                }

                inferred_return_type
            }
            Some(None) => {
                let mut span = f.span;

                if let Some(ref declared) = declared_ret_ty {
                    span = declared.span();

                    match *declared.normalize() {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsAnyKeyword,
                            ..
                        })
                        | Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsVoidKeyword,
                            ..
                        }) => {}
                        _ => errors.push(Error::ReturnRequired { span }),
                    }
                }

                Type::any(span)
            }
            None => Type::any(f.span),
        };

        Ok((
            ty::Function {
                span: f.span,
                params: f.params.iter().cloned().map(pat_to_ts_fn_param).collect(),
                type_params: f.type_params.clone().map(From::from),
                ret_ty: box declared_ret_ty
                    .map(|ty| ty.to_static().owned())
                    .unwrap_or(inferred_return_type.to_static().owned()),
            }
            .into(),
            Error::Errors {
                span: f.span,
                errors,
            },
        ))
    }

    fn check_method_call<'a>(
        &self,
        span: Span,
        c: MethodSignature<'a>,
        args: &[ExprOrSpread],
    ) -> Result<TypeRef<'a>, Error> {
        // Validate arguments
        for (i, p) in c.params.into_iter().enumerate() {
            match p {
                TsFnParam::Ident(Ident { type_ann, .. })
                | TsFnParam::Array(ArrayPat { type_ann, .. })
                | TsFnParam::Rest(RestPat { type_ann, .. })
                | TsFnParam::Object(ObjectPat { type_ann, .. }) => {
                    let lhs = match type_ann.map(Type::from) {
                        Some(lhs) => Some(self.expand_type(span, Cow::Owned(lhs))?),
                        None => None,
                    };
                    if let Some(lhs) = lhs {
                        // TODO: Handle spread
                        // TODO: Validate optional parameters
                        if args.len() > i {
                            let args_ty = self.type_of(&args[i].expr)?;
                            self.assign(&lhs, &*args_ty, args[i].span())?;
                        }
                    }
                }
            }
        }

        return Ok(c.ret_ty.unwrap_or_else(|| Type::any(span).owned()));
    }

    /// Calculates the return type of a new /call expression.
    ///
    /// Called only from [type_of_expr]
    fn extract_call_new_expr_member<'e>(
        &'e self,
        callee: &Expr,
        kind: ExtractKind,
        args: &[ExprOrSpread],
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Result<TypeRef<'e>, Error> {
        let span = callee.span();

        match *callee {
            Expr::Ident(ref i) if i.sym == js_word!("require") => {
                if let Some(dep) = self.resolved_imports.get(
                    &args
                        .iter()
                        .cloned()
                        .map(|arg| match arg {
                            ExprOrSpread { spread: None, expr } => match *expr {
                                Expr::Lit(Lit::Str(Str { value, .. })) => value.clone(),
                                _ => unimplemented!("dynamic import: require()"),
                            },
                            _ => unimplemented!("error reporting: spread element in require()"),
                        })
                        .next()
                        .unwrap(),
                ) {
                    let dep = dep.clone();
                    unimplemented!("dep: {:#?}", dep);
                }

                // if let Some(Type::Enum(ref e)) = self.scope.find_type(&i.sym) {
                //     return Ok(TsType::TsTypeRef(TsTypeRef {
                //         span,
                //         type_name: TsEntityName::Ident(i.clone()),
                //         type_params: None,
                //     })
                //     .into());
                // }
                Err(Error::UndefinedSymbol { span: i.span() })
            }

            Expr::Member(MemberExpr {
                obj: ExprOrSuper::Expr(ref obj),
                ref prop,
                computed,
                ..
            }) => {
                let is_key_eq_prop = |e: &Expr| {
                    let v = match *e {
                        Expr::Ident(ref i) => &i.sym,
                        Expr::Lit(Lit::Str(ref s)) => &s.value,
                        _ => return false,
                    };

                    let p = match **prop {
                        Expr::Ident(ref i) => &i.sym,
                        Expr::Lit(Lit::Str(ref s)) if computed => &s.value,
                        _ => return false,
                    };

                    v == p
                };

                macro_rules! search_members_for_prop {
                    ($members:expr) => {{
                        // Candidates of the method call.
                        //
                        // 4 is just an unscientific guess
                        // TODO: Use smallvec
                        let mut candidates = Vec::with_capacity(4);

                        macro_rules! check {
                            ($m:expr) => {{
                                match $m {
                                    TypeElement::Method(ref m) if kind == ExtractKind::Call => {
                                        // We are interested only on methods named `prop`
                                        if is_key_eq_prop(&m.key) {
                                            candidates.push(m.clone());
                                        }
                                    }

                                    _ => {}
                                }
                            }};
                        }

                        for m in $members {
                            check!(m);
                        }

                        {
                            // Handle methods from `interface Object`
                            let i = builtin_types::get_type(self.libs, span, &js_word!("Object"))
                                .expect("`interface Object` is must");
                            let methods = match i {
                                Type::Static(Static {
                                    ty: Type::Interface(i),
                                    ..
                                }) => &*i.body,

                                _ => &[],
                            };

                            // TODO: Remove clone
                            for m in methods.into_iter().map(|v| v.clone().static_cast()) {
                                check!(m);
                            }
                        }

                        match candidates.len() {
                            0 => {
                                unimplemented!("no method with same name\nMembers: {:?}", $members)
                            }
                            1 => {
                                // TODO:
                                return self.check_method_call(
                                    span,
                                    candidates.into_iter().next().unwrap(),
                                    args,
                                );
                            }
                            _ => {
                                //
                                for c in candidates {
                                    if c.params.len() == args.len() {
                                        return self.check_method_call(span, c, args);
                                    }
                                }

                                unimplemented!(
                                    "multiple methods with same name and same number of arguments"
                                )
                            }
                        }
                    }};
                }

                {
                    // Handle toString()
                    macro_rules! handle {
                        () => {{
                            return Ok(Cow::Owned(
                                TsKeywordType {
                                    span,
                                    kind: TsKeywordTypeKind::TsStringKeyword,
                                }
                                .into(),
                            ));
                        }};
                    }
                    match **prop {
                        Expr::Ident(Ident {
                            sym: js_word!("toString"),
                            ..
                        }) if !computed => handle!(),
                        Expr::Lit(Lit::Str(Str {
                            value: js_word!("toString"),
                            ..
                        })) => handle!(),

                        _ => {}
                    }
                }

                // Handle member expression
                let obj_type = self.type_of(obj)?.generalize_lit();

                // Example: `TypeRef(console)` -> `Interface(Console)`
                let obj_type = self.expand_type(span, obj_type)?;

                let obj_type = match *obj_type.normalize() {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsNumberKeyword,
                        ..
                    }) => builtin_types::get_type(self.libs, span, &js_word!("Number"))
                        .expect("Builtin type named 'Number' should exist")
                        .owned(),
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsStringKeyword,
                        ..
                    }) => builtin_types::get_type(self.libs, span, &js_word!("String"))
                        .expect("Builtin type named 'String' should exist")
                        .owned(),
                    _ => obj_type,
                };

                match *obj_type.normalize() {
                    Type::Function(ref f) if kind == ExtractKind::Call => {
                        //
                        return Ok(*f.ret_ty.clone());
                    }

                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsAnyKeyword,
                        ..
                    }) => {
                        return Ok(Type::any(span).into_cow());
                    }

                    Type::Interface(ref i) => {
                        // TODO: Check parent interface
                        search_members_for_prop!(i.body.iter());
                    }

                    Type::TypeLit(ref t) => {
                        search_members_for_prop!(t.members.iter());
                    }

                    Type::Class(Class { ref body, .. }) => {
                        for member in body.iter() {
                            match *member {
                                ClassMember::Method(Method {
                                    ref key,
                                    ref ret_ty,
                                    ..
                                }) => {
                                    if prop_name_to_expr(key).eq_ignore_span(&*prop) {
                                        return Ok(ret_ty.clone().to_static().owned());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsSymbolKeyword,
                        ..
                    }) => {
                        if let Ok(ty) =
                            builtin_types::get_type(self.libs, span, &js_word!("Symbol"))
                        {
                            return Ok(ty.owned());
                        }
                    }

                    _ => {}
                }

                if computed {
                    unimplemented!("typeof(CallExpr): {:?}[{:?}]()", callee, prop)
                } else {
                    println!("extract_call_or_new_expr: \nobj_type: {:?}", obj_type,);

                    Err(if kind == ExtractKind::Call {
                        Error::NoCallSignature {
                            span,
                            callee: self.type_of(callee)?.to_static(),
                        }
                    } else {
                        Error::NoNewSignature {
                            span,
                            callee: self.type_of(callee)?.to_static(),
                        }
                    })
                }
            }
            _ => {
                let ty = self.type_of(callee)?;
                println!("before extract: {:?}", ty);
                let ty = self.expand_type(span, ty)?;

                Ok(self.extract(span, &ty, kind, args, type_args)?.into_cow())
            }
        }
    }

    fn extract<'a>(
        &'a self,
        span: Span,
        ty: &Type<'a>,
        kind: ExtractKind,
        args: &[ExprOrSpread],
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Result<Type, Error> {
        if cfg!(debug_assertions) {
            match *ty.normalize() {
                Type::Simple(ref s) => match **s {
                    TsType::TsTypeRef(ref s) => unreachable!("TypeRef: {:#?}", s),
                    _ => {}
                },
                _ => {}
            }
        }

        macro_rules! ret_err {
            () => {{
                match kind {
                    ExtractKind::Call => {
                        return Err(Error::NoCallSignature {
                            span,
                            callee: ty.to_static(),
                        })
                    }
                    ExtractKind::New => {
                        return Err(Error::NoNewSignature {
                            span,
                            callee: ty.to_static(),
                        })
                    }
                }
            }};
        }

        /// Search for members and returns if there's a match
        macro_rules! search_members {
            ($members:expr) => {{
                for member in &$members {
                    match *member {
                        TypeElement::Call(CallSignature {
                            ref params,
                            ref type_params,
                            ref ret_ty,
                            ..
                        }) if kind == ExtractKind::Call => {
                            //
                            match self.try_instantiate_simple(
                                span,
                                ty.span(),
                                &ret_ty.as_ref().unwrap_or(&Type::any(span).owned()),
                                params,
                                type_params.as_ref(),
                                args,
                                type_args,
                            ) {
                                Ok(v) => return Ok(v),
                                Err(..) => {}
                            };
                        }

                        TypeElement::Constructor(ConstructorSignature {
                            ref params,
                            ref type_params,
                            ref ret_ty,
                            ..
                        }) if kind == ExtractKind::New => {
                            match self.try_instantiate_simple(
                                span,
                                ty.span(),
                                &ret_ty.as_ref().unwrap_or(&Type::any(span).owned()),
                                params,
                                type_params.as_ref(),
                                args,
                                type_args,
                            ) {
                                Ok(v) => return Ok(v),
                                Err(..) => {
                                    // TODO: Handle error
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }};
        }

        match *ty.normalize() {
            Type::Static(..) => unreachable!("normalize should handle Type::Static"),

            Type::Keyword(TsKeywordType {
                kind: TsKeywordTypeKind::TsAnyKeyword,
                ..
            }) => return Ok(Type::any(span)),

            Type::Keyword(TsKeywordType {
                kind: TsKeywordTypeKind::TsUnknownKeyword,
                ..
            }) => return Err(Error::Unknown { span }),

            Type::Function(ref f) if kind == ExtractKind::Call => {
                self.try_instantiate(span, ty.span(), &*f, args, type_args)
            }

            // Type::Constructor(ty::Constructor {
            //     ref params,
            //     ref type_params,
            //     ref ret_ty,
            //     ..
            // }) if kind == ExtractKind::New => self.try_instantiate_simple(
            //     span,
            //     ty.span(),
            //     &ret_ty,
            //     params,
            //     type_params.as_ref(),
            //     args,
            //     type_args,
            // ),
            Type::Union(ref u) => {
                let mut errors = vec![];
                for ty in &u.types {
                    match self.extract(span, &ty, kind, args, type_args) {
                        Ok(ty) => return Ok(ty),
                        Err(err) => errors.push(err),
                    }
                }

                Err(Error::UnionError { span, errors })
            }

            Type::Interface(ref i) => {
                // Search for methods
                search_members!(i.body);

                ret_err!()
            }

            Type::TypeLit(ref l) => {
                search_members!(l.members);

                ret_err!()
            }

            Type::Class(ref cls) if kind == ExtractKind::New => {
                // TODO: Remove clone
                return Ok(ClassInstance {
                    span,
                    cls: cls.clone(),
                    type_args: type_args.cloned().map(From::from),
                }
                .into());
            }

            Type::Simple(ref sty) => match **sty {
                TsType::TsTypeQuery(TsTypeQuery {
                    expr_name:
                        TsTypeQueryExpr::TsEntityName(TsEntityName::Ident(Ident { ref sym, .. })),
                    ..
                }) => {
                    if self.scope.find_declaring_fn(sym) {
                        return Ok(Type::any(span));
                    }

                    ret_err!();
                }
                _ => ret_err!(),
            },

            _ => ret_err!(),
        }
    }

    fn try_instantiate_simple<'a>(
        &'a self,
        span: Span,
        callee_span: Span,
        ret_type: &Type<'a>,
        param_decls: &[TsFnParam],
        decl: Option<&TypeParamDecl>,
        args: &[ExprOrSpread],
        _: Option<&TsTypeParamInstantiation>,
    ) -> Result<Type<'a>, Error> {
        {
            // let type_params_len = ty_params_decl.map(|decl|
            // decl.params.len()).unwrap_or(0); let type_args_len = i.map(|v|
            // v.params.len()).unwrap_or(0);

            // // TODO: Handle multiple definitions
            // let min = ty_params_decl
            //     .map(|decl| decl.params.iter().filter(|p|
            // p.default.is_none()).count())
            //     .unwrap_or(type_params_len);

            // let expected = min..=type_params_len;
            // if !expected.contains(&type_args_len) {
            //     return Err(Error::WrongTypeParams {
            //         span,
            //         callee: callee_span,
            //         expected,
            //         actual: type_args_len,
            //     });
            // }
        }

        {
            // TODO: Handle default parameters
            // TODO: Handle multiple definitions

            let min = param_decls
                .iter()
                .filter(|p| match p {
                    TsFnParam::Ident(Ident { optional: true, .. }) => false,
                    _ => true,
                })
                .count();

            let expected = min..=param_decls.len();
            if !expected.contains(&args.len()) {
                return Err(Error::WrongParams {
                    span,
                    callee: callee_span,
                    expected,
                    actual: args.len(),
                });
            }
        }

        if let Some(..) = decl {
            unimplemented!(
                "try_instantiate should be used instead of try_instantiate_simple as type \
                 parameter is deefined on the function"
            )
        } else {
            Ok(ret_type.clone())
        }
    }

    fn try_instantiate<'a>(
        &'a self,
        span: Span,
        callee_span: Span,
        fn_type: &ty::Function<'a>,
        args: &[ExprOrSpread],
        i: Option<&TsTypeParamInstantiation>,
    ) -> Result<Type<'a>, Error> {
        let param_decls = &fn_type.params;
        let decl = &fn_type.type_params;
        let type_params = if let Some(ref type_params) = fn_type.type_params {
            type_params
        } else {
            // TODO: Report an error if i is not None
            return Ok((*fn_type.ret_ty).clone().into_owned());
        };

        {
            // TODO: Handle default parameters
            // TODO: Handle multiple definitions

            let min = param_decls
                .iter()
                .filter(|p| match p {
                    TsFnParam::Ident(Ident { optional: true, .. }) => false,
                    _ => true,
                })
                .count();

            let expected = min..=param_decls.len();
            if !expected.contains(&args.len()) {
                return Err(Error::WrongParams {
                    span,
                    callee: callee_span,
                    expected,
                    actual: args.len(),
                });
            }
        }

        let v;

        let i = match i {
            Some(i) => i,
            None => {
                v = self.infer_arg_types(args, &type_params, &fn_type.params)?;
                &v
            }
        };

        if let Some(ref decl) = decl {
            // To handle
            //
            // function foo<T extends "foo">(f: (x: T) => T) {
            //     return f;
            // }
            //
            // we should expand the whole function, because return type contains type
            // parameter declared on the function.
            let expanded_fn_type = self
                .expand_type_params(i, decl, Cow::Owned(Type::Function(fn_type.clone())))?
                .into_owned();
            let expanded_fn_type = match expanded_fn_type {
                Type::Function(f) => f,
                _ => unreachable!(),
            };
            Ok((*expanded_fn_type.ret_ty).clone().into_owned())
        } else {
            Ok((*fn_type.ret_ty).clone().into_owned())
        }
    }

    /// Expands
    ///
    ///   - Type alias
    pub(super) fn expand_type<'t>(
        &'t self,
        span: Span,
        ty: TypeRef<'t>,
    ) -> Result<TypeRef<'t>, Error> {
        // println!("({}) expand({:?})", self.scope.depth(), ty);

        match *ty {
            Type::Static(s) => return self.expand_type(span, s.ty.static_cast()),
            Type::Simple(ref s_ty) => {
                macro_rules! verify {
                    ($ty:expr) => {{
                        if cfg!(debug_assertions) {
                            match $ty.normalize() {
                                Type::Simple(ref s) => match **s {
                                    TsType::TsTypeRef(..) => unreachable!(),
                                    _ => {}
                                },
                                _ => {}
                            }
                        }
                    }};
                }

                match **s_ty {
                    TsType::TsTypeRef(TsTypeRef {
                        ref type_name,
                        ref type_params,
                        ..
                    }) => {
                        match *type_name {
                            TsEntityName::Ident(ref i) => {
                                // Check for builtin types
                                if let Ok(ty) = builtin_types::get_type(self.libs, span, &i.sym) {
                                    verify!(ty);
                                    return self.expand_type(span, ty.owned());
                                }

                                // Handle enum
                                if let Some(ref ty) = self.find_type(&i.sym) {
                                    match ty.normalize() {
                                        Type::Enum(..) => {
                                            if let Some(..) = *type_params {
                                                return Err(Error::NotGeneric { span });
                                            }
                                            verify!(ty);
                                            return Ok(ty.static_cast());
                                        }

                                        Type::Param(..) => {
                                            if let Some(..) = *type_params {
                                                return Err(Error::NotGeneric { span });
                                            }

                                            verify!(ty);
                                            return Ok(ty.static_cast());
                                        }

                                        Type::Interface(..) | Type::Class(..) => {
                                            // TODO: Handle type parameters
                                            verify!(ty);
                                            return Ok(ty.static_cast());
                                        }

                                        Type::Alias(Alias {
                                            type_params: None,
                                            ref ty,
                                            ..
                                        }) => {
                                            verify!(ty);
                                            return Ok(ty.static_cast());
                                        }

                                        // Expand type parameters.
                                        Type::Alias(Alias {
                                            type_params: Some(ref tps),
                                            ref ty,
                                            ..
                                        }) => {
                                            let ty = if let Some(i) = type_params {
                                                self.expand_type_params(
                                                    i,
                                                    tps,
                                                    Cow::Borrowed(&**ty),
                                                )?
                                            } else {
                                                *ty.clone()
                                            };
                                            let ty = self.expand_type(span, ty.static_cast())?;

                                            verify!(ty);
                                            return Ok(ty.to_static().owned());
                                        }

                                        _ => unimplemented!(
                                            "Handling result of find_type() -> {:#?}",
                                            ty
                                        ),
                                    }
                                } else {
                                    println!("Failed to find type: {}", i.sym)
                                }
                            }

                            // Handle enum variant type.
                            //
                            //  let a: StringEnum.Foo = x;
                            TsEntityName::TsQualifiedName(box TsQualifiedName {
                                left: TsEntityName::Ident(ref left),
                                ref right,
                            }) => {
                                if left.sym == js_word!("void") {
                                    return Ok(Type::any(span).into_cow());
                                }

                                if let Some(ref ty) = self.scope.find_type(&left.sym) {
                                    match *ty {
                                        Type::Enum(..) => {
                                            return Ok(EnumVariant {
                                                span,
                                                enum_name: left.sym.clone(),
                                                name: right.sym.clone(),
                                            }
                                            .into_cow());
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {
                                unimplemented!("TsEntityName: {:?}", type_name);
                            }
                        }

                        return Err(Error::NameNotFound {
                            span: type_name.span(),
                        });
                    }

                    TsType::TsTypeQuery(TsTypeQuery {
                        span,
                        expr_name: TsTypeQueryExpr::TsEntityName(ref expr_name),
                        ..
                    }) => return self.type_of_ts_entity_name(span, expr_name, None),

                    _ => {
                        return Ok(Cow::Owned((*s_ty).clone().into()));
                    }
                }
            }

            _ => {}
        }

        let ty = match ty.into_owned() {
            Type::Union(Union { span, types }) => {
                let v = types
                    .into_iter()
                    .map(|ty| Ok(self.expand_type(span, ty)?.into_owned()))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Type::union(v).into_cow());
            }
            Type::Intersection(Intersection { span, types }) => {
                return Ok(Intersection {
                    span,
                    types: types
                        .into_iter()
                        .map(|ty| Ok(self.expand_type(span, ty)?))
                        .collect::<Result<_, _>>()?,
                }
                .into_cow());
            }

            Type::Array(Array {
                span,
                box elem_type,
            }) => {
                let elem_type = box self.expand_type(elem_type.span(), elem_type)?;
                return Ok(Array { span, elem_type }.into_cow());
            }

            // type Baz = "baz"
            // let a: ["foo", "bar", Baz] = ["foo", "bar", "baz"];
            Type::Tuple(Tuple { span, types }) => {
                return Ok(Tuple {
                    span,
                    types: types
                        .into_iter()
                        .map(|v| self.expand_type(v.span(), v))
                        .collect::<Result<_, _>>()?,
                }
                .into_cow());
            }

            Type::Alias(Alias {
                type_params: None,
                ty,
                ..
            }) => {
                return Ok(*ty);
            }

            Type::Function(ty::Function {
                span,
                type_params,
                params,
                ret_ty,
            }) => {
                return Ok(ty::Function {
                    span,
                    type_params,
                    params,
                    ret_ty: box self.expand_type(span, *ret_ty)?,
                }
                .into_cow());
            }

            ty => ty,
        };

        Ok(ty.into_cow())
    }
}

fn prop_key_to_expr(p: &Prop) -> Box<Expr> {
    match *p {
        Prop::Shorthand(ref i) => box Expr::Ident(i.clone()),
        Prop::Assign(AssignProp { ref key, .. }) => box Expr::Ident(key.clone()),
        Prop::Getter(GetterProp { ref key, .. })
        | Prop::KeyValue(KeyValueProp { ref key, .. })
        | Prop::Method(MethodProp { ref key, .. })
        | Prop::Setter(SetterProp { ref key, .. }) => prop_name_to_expr(key),
    }
}

fn prop_name_to_expr(key: &PropName) -> Box<Expr> {
    match *key {
        PropName::Computed(ref p) => p.expr.clone(),
        PropName::Ident(ref ident) => box Expr::Ident(ident.clone()),
        PropName::Str(ref s) => box Expr::Lit(Lit::Str(Str { ..s.clone() })),
        PropName::Num(ref s) => box Expr::Lit(Lit::Num(Number { ..s.clone() })),
    }
}

fn negate(ty: Type) -> Type {
    match ty {
        Type::Lit(TsLitType { ref lit, span }) => match *lit {
            TsLit::Bool(v) => {
                return Type::Lit(TsLitType {
                    lit: TsLit::Bool(Bool {
                        value: !v.value,
                        ..v
                    }),
                    span,
                });
            }
            TsLit::Number(v) => {
                return Type::Lit(TsLitType {
                    lit: TsLit::Bool(Bool {
                        value: v.value != 0.0,
                        span: v.span,
                    }),
                    span,
                });
            }
            TsLit::Str(ref v) => {
                return Type::Lit(TsLitType {
                    lit: TsLit::Bool(Bool {
                        value: v.value != js_word!(""),
                        span: v.span,
                    }),
                    span,
                });
            }
        },

        _ => {}
    }

    TsKeywordType {
        span: ty.span(),
        kind: TsKeywordTypeKind::TsBooleanKeyword,
    }
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtractKind {
    Call,
    New,
}

impl Fold<SeqExpr> for Analyzer<'_, '_> {
    fn fold(&mut self, mut expr: SeqExpr) -> SeqExpr {
        log_fold!(expr);

        let first_span = expr.exprs[0].span();

        for expr in expr.exprs.drain(..expr.exprs.len() - 1) {
            let span = expr.span();
            let expr = expr.fold_with(self);

            match *expr {
                Expr::Ident(..)
                | Expr::Lit(..)
                | Expr::Arrow(..)
                | Expr::Unary(UnaryExpr {
                    op: op!(unary, "-"),
                    ..
                })
                | Expr::Unary(UnaryExpr {
                    op: op!(unary, "+"),
                    ..
                })
                | Expr::Unary(UnaryExpr { op: op!("!"), .. })
                    if !self.rule.allow_unreachable_code =>
                {
                    self.info.errors.push(Error::UselessSeqExpr {
                        span: span.with_lo(first_span.lo()),
                    });
                }

                _ => match self.type_of(&expr) {
                    Ok(..) => {}
                    Err(err) => self.info.errors.push(err),
                },
            }
        }

        if expr.exprs.len() != 0 {
            let last = expr.exprs.pop().unwrap();
            expr.exprs.push(last.fold_with(self));
        }

        expr
    }
}

impl Analyzer<'_, '_> {
    pub(super) fn type_of_ts_entity_name(
        &self,
        span: Span,
        n: &TsEntityName,
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Result<TypeRef, Error> {
        match *n {
            TsEntityName::Ident(ref i) => self.type_of_ident(i, TypeOfMode::RValue),
            TsEntityName::TsQualifiedName(ref qname) => {
                let obj_ty = self.type_of_ts_entity_name(span, &qname.left, None)?;

                self.access_property(
                    span,
                    obj_ty,
                    &Expr::Ident(qname.right.clone()),
                    false,
                    TypeOfMode::RValue,
                )
            }
        }
    }
}