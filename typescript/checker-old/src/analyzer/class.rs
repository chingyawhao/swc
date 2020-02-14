use super::{
    expr::TypeOfMode,
    scope::ScopeKind,
    util::{is_prop_name_eq, VarVisitor},
    Analyzer,
};
use crate::{
    analyzer::{props::ComputedPropMode, util::ResultExt, Ctx},
    errors::{Error, Errors},
    swc_common::VisitWith,
    ty,
    ty::{FnParam, Operator, Type},
    validator::{Validate, ValidateWith},
    ValidationResult,
};
use macros::{validator, validator_method};
use std::mem::replace;
use swc_atoms::js_word;
use swc_common::{Span, Spanned, Visit, DUMMY_SP};
use swc_ecma_ast::*;

#[validator]
impl Validate<ClassProp> for Analyzer<'_, '_> {
    type Output = ValidationResult<ty::ClassProperty>;

    fn validate(&mut self, p: &ClassProp) -> Self::Output {
        self.record(p);

        // Verify key if key is computed
        if p.computed {
            self.validate_computed_prop_key(p.span, &p.key);
        }

        let value = {
            let ty = try_opt!(p.type_ann.validate_with(self));
            let value_ty = try_opt!(self.validate(&p.value));

            ty.or_else(|| value_ty)
        };

        Ok(ty::ClassProperty {
            span: p.span,
            key: p.key.clone(),
            value,
            is_static: p.is_static,
            computed: p.computed,
            accessibility: p.accessibility,
            is_abstract: p.is_abstract,
            is_optional: p.is_optional,
            readonly: p.readonly,
            definite: p.definite,
        })
    }
}

#[validator]
impl Validate<Constructor> for Analyzer<'_, '_> {
    type Output = ValidationResult<ty::Constructor>;

    fn validate(&mut self, c: &Constructor) -> Self::Output {
        self.record(c);

        let c_span = c.span();

        self.with_child(ScopeKind::Fn, Default::default(), |child| {
            let Constructor { params, .. } = c;

            {
                // Validate params
                // TODO: Move this to parser
                let mut has_optional = false;
                for p in params {
                    if has_optional {
                        child.info.errors.push(Error::TS1016 { span: p.span() });
                    }

                    match *p {
                        PatOrTsParamProp::Pat(Pat::Ident(Ident { optional, .. })) => {
                            if optional {
                                has_optional = true;
                            }
                        }
                        _ => {}
                    }
                }
            }

            for param in params {
                let mut names = vec![];

                let mut visitor = VarVisitor { names: &mut names };

                param.visit_with(&mut visitor);

                child.scope.declaring.extend(names.clone());

                match param {
                    PatOrTsParamProp::Pat(ref pat) => {
                        match child.declare_vars(VarDeclKind::Let, pat) {
                            Ok(()) => {}
                            Err(err) => {
                                child.info.errors.push(err);
                            }
                        }
                    }
                    PatOrTsParamProp::TsParamProp(ref param) => match param.param {
                        TsParamPropParam::Ident(ref i)
                        | TsParamPropParam::Assign(AssignPat {
                            left: box Pat::Ident(ref i),
                            ..
                        }) => {
                            let ty = try_opt!(child.validate(&i.type_ann));
                            let ty = try_opt!(ty.map(|ty| child.expand(ty.span(), ty)));
                            //let ty = match ty {
                            //    Some(ty) => match child.expand_type(i.span, ty) {
                            //        Ok(ty) => Some(ty),
                            //        Err(err) => {
                            //            child.info.errors.push(err);
                            //            Some(Type::any(i.span))
                            //        }
                            //    },
                            //    None => None,
                            //};

                            match child.declare_var(
                                i.span,
                                VarDeclKind::Let,
                                i.sym.clone(),
                                ty,
                                true,
                                false,
                            ) {
                                Ok(()) => {}
                                Err(err) => {
                                    child.info.errors.push(err);
                                }
                            }
                        }
                        _ => unreachable!(),
                    },
                }

                child.scope.remove_declaring(names);
            }

            Ok(ty::Constructor {
                span: c.span,
                params: c
                    .params
                    .iter()
                    .map(|v| match v {
                        PatOrTsParamProp::TsParamProp(TsParamProp {
                            param: TsParamPropParam::Ident(i),
                            ..
                        }) => TsFnParam::Ident(i.clone()),
                        PatOrTsParamProp::TsParamProp(TsParamProp {
                            param: TsParamPropParam::Assign(AssignPat { left: box pat, .. }),
                            ..
                        })
                        | PatOrTsParamProp::Pat(pat) => from_pat(pat.clone()),
                    })
                    .map(|param| child.validate(&param))
                    .collect::<Result<_, _>>()?,
            })
        })
    }
}

#[validator]
impl Validate<TsFnParam> for Analyzer<'_, '_> {
    type Output = ValidationResult<ty::FnParam>;

    fn validate(&mut self, p: &TsFnParam) -> Self::Output {
        let span = p.span();

        macro_rules! ty {
            ($e:expr) => {{
                let e: Option<_> = try_opt!($e.validate_with(self));
                e.unwrap_or_else(|| Type::any(span))
            }};
        }

        Ok(match p {
            TsFnParam::Ident(i) => ty::FnParam {
                span,
                required: !i.optional,
                ty: ty!(i.type_ann),
            },
            TsFnParam::Array(p) => FnParam {
                span,
                required: true,
                ty: ty!(p.type_ann),
            },
            TsFnParam::Rest(p) => FnParam {
                span,
                required: false,
                ty: ty!(p.type_ann),
            },
            TsFnParam::Object(p) => FnParam {
                span,
                required: true,
                ty: ty!(p.type_ann),
            },
        })
    }
}

#[validator]
impl Validate<ClassMethod> for Analyzer<'_, '_> {
    type Output = ValidationResult<ty::Method>;

    fn validate(&mut self, c: &ClassMethod) -> Self::Output {
        self.record(c);

        let c_span = c.span();
        let key_span = c.key.span();

        let (params, type_params, declared_ret_ty, inferred_ret_ty) = self.with_child(
            ScopeKind::Fn,
            Default::default(),
            |child| -> ValidationResult<_> {
                {
                    // It's error if abstract method has a body

                    if c.is_abstract && c.function.body.is_some() {
                        child.info.errors.push(Error::TS1318 { span: key_span });
                    }
                }

                {
                    // Validate params
                    // TODO: Move this to parser
                    let mut has_optional = false;
                    for p in &c.function.params {
                        if has_optional {
                            child.info.errors.push(Error::TS1016 { span: p.span() });
                        }

                        match *p {
                            Pat::Ident(Ident { optional, .. }) => {
                                if optional {
                                    has_optional = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }

                let params = c.function.params.validate_with(child)?;

                let type_params = try_opt!(c.function.type_params.validate_with(child));
                if (c.kind == MethodKind::Getter || c.kind == MethodKind::Setter)
                    && type_params.is_some()
                {
                    child.info.errors.push(Error::TS1094 { span: key_span })
                }

                c.key.visit_with(child);
                // c.function.visit_children(child);

                if child.ctx.in_declare && c.function.body.is_some() {
                    child.info.errors.push(Error::TS1183 { span: key_span })
                }

                if c.kind == MethodKind::Setter && c.function.return_type.is_some() {
                    child.info.errors.push(Error::TS1095 { span: key_span })
                }

                let declared_ret_ty = try_opt!(c.function.return_type.validate_with(child));

                let inferred_ret_ty = match c
                    .function
                    .body
                    .as_ref()
                    .map(|bs| child.visit_stmts_for_return(&bs.stmts))
                {
                    Some(Ok(ty)) => ty,
                    Some(err) => err?,
                    None => None,
                };

                Ok((params, type_params, declared_ret_ty, inferred_ret_ty))
            },
        )?;

        if c.kind == MethodKind::Getter && c.function.body.is_some() {
            // Inferred return type.

            // getter property must have return statements.
            if let None = inferred_ret_ty {
                self.info.errors.push(Error::TS2378 { span: key_span });
            }
        }

        let ret_ty =
            declared_ret_ty.unwrap_or_else(|| inferred_ret_ty.unwrap_or_else(|| Type::any(c_span)));

        Ok(ty::Method {
            span: c_span,
            key: c.key.clone(),
            is_static: c.is_static,
            type_params,
            params,
            ret_ty: box ret_ty,
        })
    }
}

#[validator]
impl Validate<ClassMember> for Analyzer<'_, '_> {
    type Output = ValidationResult<Option<ty::ClassMember>>;

    fn validate(&mut self, m: &ClassMember) -> Self::Output {
        Ok(match m {
            swc_ecma_ast::ClassMember::PrivateMethod(_)
            | swc_ecma_ast::ClassMember::PrivateProp(_) => None,

            swc_ecma_ast::ClassMember::Constructor(v) => {
                Some(ty::ClassMember::Constructor(v.validate_with(self)?))
            }
            swc_ecma_ast::ClassMember::Method(v) => {
                Some(ty::ClassMember::Method(v.validate_with(self)?))
            }
            swc_ecma_ast::ClassMember::ClassProp(v) => {
                Some(ty::ClassMember::Property(v.validate_with(self)?))
            }
            swc_ecma_ast::ClassMember::TsIndexSignature(v) => {
                Some(ty::ClassMember::IndexSignature(v.validate_with(self)?))
            }
        })
    }
}

impl Analyzer<'_, '_> {
    fn validate_class_members(
        &mut self,
        c: &Class,
        declare: bool,
    ) -> ValidationResult<Vec<ty::ClassMember>> {
        // Report errors for code like
        //
        //      class C {
        //           foo();
        //      }

        let mut errors = vec![];
        // Span of name
        let mut spans = vec![];
        let mut name: Option<&PropName> = None;

        if !declare {
            for m in &c.body {
                macro_rules! check {
                    ($m:expr, $body:expr) => {{
                        let m = $m;

                        match m.key {
                            PropName::Computed(..) => continue,
                            _ => {}
                        }

                        if $body.is_none() {
                            if name.is_some() && !is_prop_name_eq(&name.unwrap(), &m.key) {
                                for span in replace(&mut spans, vec![]) {
                                    errors.push(Error::TS2391 { span });
                                }
                            }

                            name = Some(&m.key);
                            spans.push(m.key.span());
                        } else {
                            if name.is_none() || is_prop_name_eq(&name.unwrap(), &m.key) {
                                // TODO: Verify parameters

                                spans = vec![];
                                name = None;
                            } else {
                                let constructor_name =
                                    PropName::Ident(Ident::new(js_word!("constructor"), DUMMY_SP));

                                if is_prop_name_eq(&name.unwrap(), &constructor_name) {
                                    for span in replace(&mut spans, vec![]) {
                                        errors.push(Error::TS2391 { span });
                                    }
                                } else if is_prop_name_eq(&m.key, &constructor_name) {
                                    for span in replace(&mut spans, vec![]) {
                                        errors.push(Error::TS2389 { span });
                                    }
                                } else {
                                    spans = vec![];

                                    errors.push(Error::TS2389 { span: m.key.span() });
                                }

                                name = None;
                            }
                        }
                    }};
                }

                match *m {
                    ClassMember::Constructor(ref m) => check!(m, m.body),
                    ClassMember::Method(
                        ref
                        m
                        @
                        ClassMethod {
                            is_abstract: false, ..
                        },
                    ) => check!(m, m.function.body),
                    _ => {}
                }
            }

            // Class definition ended with `foo();`
            for span in replace(&mut spans, vec![]) {
                errors.push(Error::TS2391 { span });
            }
        }

        self.info.errors.extend(errors);

        Ok(vec![])
    }

    #[validator_method]
    pub(super) fn validate_computed_prop_key(&mut self, span: Span, key: &Expr) {
        if self.is_builtin {
            // We don't need to validate builtins
            return;
        }

        let mut errors = Errors::default();
        let is_symbol_access = match *key {
            Expr::Member(MemberExpr {
                obj:
                    ExprOrSuper::Expr(box Expr::Ident(Ident {
                        sym: js_word!("Symbol"),
                        ..
                    })),
                ..
            }) => true,
            _ => false,
        };

        let ty = match self.validate(&key).map(|ty| ty.respan(span)) {
            Ok(ty) => ty,
            Err(err) => {
                match err {
                    Error::TS2585 { span } => Err(Error::TS2585 { span })?,
                    _ => {}
                }

                errors.push(err);

                Type::any(span)
            }
        };

        match *ty.normalize() {
            Type::Lit(..) => {}
            Type::Operator(Operator {
                op: TsTypeOperatorOp::Unique,
                ty:
                    box Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsSymbolKeyword,
                        ..
                    }),
                ..
            }) => {}
            _ if is_symbol_access => {}
            _ => errors.push(Error::TS1166 { span }),
        }

        if !errors.is_empty() {
            Err(Error::Errors {
                span,
                errors: errors.into(),
            })?
        }
    }

    /// Should be called only from `Validate<Class>`.
    fn validate_inherited_members(&mut self, name: Option<Span>, class: &ty::Class) {
        if class.is_abstract || self.ctx.in_declare {
            return;
        }

        let name_span = name.unwrap_or_else(|| {
            // TODD: c.span().lo() + BytePos(5) (aka class token)
            class.span
        });
        let mut errors = vec![];

        let res: Result<_, Error> = try {
            if let Some(ref super_ty) = class.super_class {
                match super_ty.normalize() {
                    Type::Class(sc) => {
                        'outer: for sm in &sc.body {
                            match sm {
                                ty::ClassMember::Method(sm) => {
                                    for m in &class.body {
                                        match m {
                                            ty::ClassMember::Method(ref m) => {
                                                if !is_prop_name_eq(&m.key, &sm.key) {
                                                    continue;
                                                }

                                                // TODO: Validate parameters

                                                // TODO: Validate return type
                                                continue 'outer;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                _ => {
                                    // TODO: Verify
                                    continue 'outer;
                                }
                            }

                            errors.push(Error::TS2515 { span: name_span });

                            if sc.is_abstract {
                                // TODO: Check super class of super class
                            }
                        }
                    }
                    _ => {}
                }
            }
        };

        if let Err(err) = res {
            errors.push(err);
        }

        self.info.errors.extend(errors);
    }
}

#[validator]
impl Validate<Class> for Analyzer<'_, '_> {
    type Output = ValidationResult<ty::Class>;

    fn validate(&mut self, c: &Class) -> Self::Output {
        self.record(c);

        self.ctx.computed_prop_mode = ComputedPropMode::Class {
            has_body: !self.ctx.in_declare,
        };

        c.decorators.visit_with(self);
        self.resolve_parent_interfaces(&c.implements);
        let name = self.scope.this_class_name.take();

        // Scope is required because of type parameters.
        self.with_child(ScopeKind::Class, Default::default(), |child| {
            // We handle type parameters first.
            let type_params = try_opt!(c.type_params.validate_with(child));

            let super_class = {
                // Then, we can expand super class

                let super_type_params = try_opt!(c.super_type_params.validate_with(child));
                match &c.super_class {
                    Some(box expr) => Some(box child.validate_expr(
                        expr,
                        TypeOfMode::RValue,
                        super_type_params,
                    )?),

                    _ => None,
                }
            };

            c.implements.visit_with(child);

            // TODO: Check for implements

            // Register the class.
            child.scope.this_class_name = name.clone();

            let body = c
                .body
                .iter()
                .filter_map(|m| match child.validate(m) {
                    Ok(Some(v)) => Some(Ok(v)),
                    Ok(None) => None,
                    Err(err) => Some(Err(err)),
                })
                .collect::<Result<_, _>>()?;

            {
                // Validate constructors
                let mut constructor_spans = vec![];
                let mut constructor_required_param_count = None;

                for m in &c.body {
                    match *m {
                        ClassMember::Constructor(ref cons) => {
                            //
                            if cons.body.is_none() {
                                for p in &cons.params {
                                    match *p {
                                        PatOrTsParamProp::TsParamProp(..) => {
                                            child.info.errors.push(Error::TS2369 { span: p.span() })
                                        }
                                        _ => {}
                                    }
                                }
                            }

                            {
                                // Check parameter count
                                let required_param_count = cons
                                    .params
                                    .iter()
                                    .filter(|p| match p {
                                        PatOrTsParamProp::Pat(Pat::Ident(Ident {
                                            optional: true,
                                            ..
                                        })) => false,
                                        _ => true,
                                    })
                                    .count();

                                match constructor_required_param_count {
                                    Some(v) if required_param_count != v => {
                                        for span in constructor_spans.drain(..) {
                                            child.info.errors.push(Error::TS2394 { span })
                                        }
                                    }

                                    None => {
                                        constructor_required_param_count =
                                            Some(required_param_count)
                                    }
                                    _ => {}
                                }
                            }

                            constructor_spans.push(cons.span);
                        }

                        _ => {}
                    }
                }
            }

            let class = ty::Class {
                span: c.span,
                name,
                is_abstract: c.is_abstract,
                super_class,
                type_params,
                body,
            };

            child.validate_inherited_members(None, &class);

            Ok(class)
        })
    }
}

impl Visit<ClassExpr> for Analyzer<'_, '_> {
    fn visit(&mut self, c: &ClassExpr) {
        self.scope.this_class_name = c.ident.as_ref().map(|v| v.sym.clone());
        let ty = match c.class.validate_with(self) {
            Ok(ty) => ty.into(),
            Err(err) => {
                self.info.errors.push(err);
                Type::any(c.span())
            }
        };

        let old_this = self.scope.this.take();
        // self.scope.this = Some(ty.clone());

        let c = self
            .with_child(ScopeKind::Block, Default::default(), |analyzer| {
                if let Some(ref i) = c.ident {
                    analyzer.register_type(i.sym.clone(), ty.clone())?;

                    analyzer.validate_class_members(&c.class, false)?;

                    match analyzer.declare_var(
                        ty.span(),
                        VarDeclKind::Var,
                        i.sym.clone(),
                        Some(ty),
                        // initialized = true
                        true,
                        // declare Class does not allow multiple declarations.
                        false,
                    ) {
                        Ok(()) => {}
                        Err(err) => {
                            analyzer.info.errors.push(err);
                        }
                    }
                }

                c.visit_children(analyzer);

                Ok(())
            })
            .store(&mut self.info.errors);

        self.scope.this = old_this;
    }
}

impl Visit<ClassDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, c: &ClassDecl) {
        self.record(c);

        let ctx = Ctx { ..self.ctx };
        self.with_ctx(ctx).visit_class_decl(c);
    }
}

impl Analyzer<'_, '_> {
    fn visit_class_decl(&mut self, c: &ClassDecl) {
        c.ident.visit_with(self);

        self.validate_class_members(&c.class, c.declare)
            .store(&mut self.info.errors);

        self.scope.this_class_name = Some(c.ident.sym.clone());
        let ty = match c.class.validate_with(self) {
            Ok(ty) => ty.into(),
            Err(err) => {
                self.info.errors.push(err);
                Type::any(c.span())
            }
        };

        let old_this = self.scope.this.take();
        // self.scope.this = Some(ty.clone());

        self.register_type(c.ident.sym.clone(), ty.clone().into())
            .store(&mut self.info.errors);

        match self.declare_var(
            ty.span(),
            VarDeclKind::Var,
            c.ident.sym.clone(),
            Some(ty),
            // initialized = true
            true,
            // declare Class does not allow multiple declarations.
            false,
        ) {
            Ok(()) => {}
            Err(err) => {
                self.info.errors.push(err);
            }
        }

        self.scope.this = old_this;
    }
}

fn from_pat(pat: Pat) -> TsFnParam {
    match pat {
        Pat::Ident(v) => v.into(),
        Pat::Array(v) => v.into(),
        Pat::Rest(v) => v.into(),
        Pat::Object(v) => v.into(),
        Pat::Assign(v) => from_pat(*v.left),
        _ => unreachable!("constructor with parameter {:?}", pat),
    }
}