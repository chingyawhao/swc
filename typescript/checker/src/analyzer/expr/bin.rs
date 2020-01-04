use super::super::{
    util::{Comparator, ResultExt},
    Analyzer,
};
use crate::{
    errors::Error,
    ty::{Type, TypeRef},
    util::{EqIgnoreSpan, IntoCow},
    ValidationResult,
};
use swc_common::{Span, Spanned};
use swc_ecma_ast::*;

prevent!(BinExpr);

impl Analyzer<'_, '_> {
    fn validate_bin_inner(
        &mut self,
        span: Span,
        op: BinaryOp,
        lt: Option<TypeRef<'static>>,
        rt: Option<TypeRef<'static>>,
    ) {
        let mut errors = vec![];

        match op {
            op!("===") | op!("!==") => {
                let ls = lt.span();
                let rs = rt.span();

                if lt.is_some() && rt.is_some() {
                    let lt = lt.unwrap();
                    let rt = rt.unwrap();

                    let has_overlap = lt.eq_ignore_span(&*rt) || {
                        let c = Comparator {
                            left: &*lt,
                            right: &*rt,
                        };

                        // Check if type overlaps.
                        match c.take(|l, r| {
                            // Returns Some(()) if r may be assignable to l
                            match l {
                                Type::Lit(ref l_lit) => {
                                    // "foo" === "bar" is always false.
                                    match r {
                                        Type::Lit(ref r_lit) => {
                                            if l_lit.eq_ignore_span(&*r_lit) {
                                                Some(())
                                            } else {
                                                None
                                            }
                                        }
                                        _ => Some(()),
                                    }
                                }
                                Type::Union(ref u) => {
                                    // Check if u contains r

                                    Some(())
                                }
                                _ => None,
                            }
                        }) {
                            Some(()) => true,
                            None => false,
                        }
                    };

                    if !has_overlap {
                        errors.push(Error::NoOverlap {
                            span,
                            value: op != op!("==="),
                            left: ls,
                            right: rs,
                        })
                    }
                }
            }
            op!(bin, "+") => {
                // Validation is performed in type_of_bin_expr because
                // validation of types is required to compute type of the
                // expression.
            }
            op!("||") | op!("&&") => {
                if lt.is_some() {
                    match *lt.as_ref().unwrap().normalize() {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsVoidKeyword,
                            ..
                        }) => errors.push(Error::TS1345 { span }),
                        _ => {}
                    }
                }
            }

            op!("*")
            | op!("/")
            | op!("%")
            | op!(bin, "-")
            | op!("<<")
            | op!(">>")
            | op!(">>>")
            | op!("&")
            | op!("^")
            | op!("|") => {
                if lt.is_some() && rt.is_some() {
                    let lt = lt.unwrap();
                    let rt = rt.unwrap();

                    let mut check = |ty: &Type, is_left| match ty {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsAnyKeyword,
                            ..
                        })
                        | Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                            ..
                        })
                        | Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsBigIntKeyword,
                            ..
                        })
                        | Type::Lit(TsLitType {
                            lit: TsLit::Number(..),
                            ..
                        })
                        | Type::Enum(..)
                        | Type::EnumVariant(..) => {}

                        _ => errors.push(if is_left {
                            Error::TS2362 { span: ty.span() }
                        } else {
                            Error::TS2363 { span: ty.span() }
                        }),
                    };

                    if (op == op!("&") || op == op!("^") || op == op!("|"))
                        && match lt.normalize() {
                            Type::Keyword(TsKeywordType {
                                kind: TsKeywordTypeKind::TsBooleanKeyword,
                                ..
                            })
                            | Type::Lit(TsLitType {
                                lit: TsLit::Bool(..),
                                ..
                            }) => true,
                            _ => false,
                        }
                        && match rt.normalize() {
                            Type::Keyword(TsKeywordType {
                                kind: TsKeywordTypeKind::TsBooleanKeyword,
                                ..
                            })
                            | Type::Lit(TsLitType {
                                lit: TsLit::Bool(..),
                                ..
                            }) => true,
                            _ => false,
                        }
                    {
                        errors.push(Error::TS2447 { span });
                    } else {
                        check(&lt, true);
                        check(&rt, false);
                    }
                }
            }

            op!("in") => {
                if lt.is_some() {
                    match lt.unwrap().normalize() {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsAnyKeyword,
                            ..
                        })
                        | Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsStringKeyword,
                            ..
                        })
                        | Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                            ..
                        })
                        | Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsBigIntKeyword,
                            ..
                        })
                        | Type::Lit(TsLitType {
                            lit: TsLit::Number(..),
                            ..
                        })
                        | Type::Lit(TsLitType {
                            lit: TsLit::Str(..),
                            ..
                        })
                        | Type::Enum(..)
                        | Type::EnumVariant(..) => {}

                        _ => errors.push(Error::TS2360 { span: lt.span() }),
                    }
                }

                if rt.is_some() {
                    fn is_ok(ty: &Type) -> bool {
                        if ty.is_any() {
                            return true;
                        }

                        match ty.normalize() {
                            Type::TypeLit(..)
                            | Type::Param(..)
                            | Type::Array(..)
                            | Type::Tuple(..)
                            | Type::Interface(..)
                            | Type::Keyword(TsKeywordType {
                                kind: TsKeywordTypeKind::TsObjectKeyword,
                                ..
                            }) => true,
                            Type::Union(ref u) => u.types.iter().all(|ty| is_ok(&ty)),
                            _ => false,
                        }
                    }

                    if !is_ok(&rt.unwrap()) {
                        errors.push(Error::TS2361 { span: rt.span() })
                    }
                }
            }

            _ => {}
        }

        self.info.errors.extend(errors);
    }

    pub(super) fn validate_bin_expr(&mut self, expr: &BinExpr) -> ValidationResult {
        let BinExpr {
            span,
            op,
            ref left,
            ref right,
        } = *expr;

        let mut errors = vec![];

        let lt = self.validate_expr(&left).store(&mut errors);
        let rt = self.validate_expr(&right).store(&mut errors);

        self.validate_bin_inner(span, op, lt, rt);

        let (lt, rt) = match (lt, rt) {
            (Some(l), Some(r)) => (l, r),
            _ => Err(errors)?,
        };

        macro_rules! no_unknown {
            () => {{
                no_unknown!(lt);
                no_unknown!(rt);
            }};
            ($ty:expr) => {{
                match *$ty {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsUnknownKeyword,
                        ..
                    }) => {
                        return Err(Error::Unknown { span });
                    }
                    _ => {}
                }
            }};
        }

        match op {
            op!(bin, "+") => {
                no_unknown!();

                let c = Comparator {
                    left: (&**left, &lt),
                    right: (&**right, &rt),
                };

                if let Some(()) = c.take(|(_, lt), (_, _)| match **lt {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsUnknownKeyword,
                        ..
                    }) => Some(()),

                    _ => None,
                }) {
                    return Err(Error::Unknown { span });
                }

                match *lt {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsNumberKeyword,
                        ..
                    })
                    | Type::Lit(TsLitType {
                        lit: TsLit::Number(..),
                        ..
                    }) => match *rt {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                            ..
                        })
                        | Type::Lit(TsLitType {
                            lit: TsLit::Number(..),
                            ..
                        }) => {
                            return Ok(Type::Keyword(TsKeywordType {
                                span,
                                kind: TsKeywordTypeKind::TsStringKeyword,
                            })
                            .owned());
                        }
                        _ => {}
                    },
                    _ => {}
                }

                if let Some(()) = c.take(|(_, lt), (_, _)| match **lt {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsStringKeyword,
                        ..
                    })
                    | Type::Lit(TsLitType {
                        lit: TsLit::Str(..),
                        ..
                    }) => Some(()),

                    _ => None,
                }) {
                    return Ok(Type::Keyword(TsKeywordType {
                        span,
                        kind: TsKeywordTypeKind::TsStringKeyword,
                    })
                    .owned());
                }

                // Rule:
                //  - any + string is string
                //  - any + other is any
                if let Some(kind) = c.take(|(_, lt), (_, rt)| {
                    if lt.is_any() {
                        if rt.is_str() {
                            return Some(TsKeywordTypeKind::TsStringKeyword);
                        }
                        return Some(TsKeywordTypeKind::TsAnyKeyword);
                    }

                    None
                }) {
                    return Ok(Type::Keyword(TsKeywordType { span, kind }).owned());
                }

                if c.any(|(_, ty)| {
                    ty.is_keyword(TsKeywordTypeKind::TsUndefinedKeyword)
                        || ty.is_keyword(TsKeywordTypeKind::TsNullKeyword)
                }) {
                    return Err(Error::TS2365 { span });
                }

                // Rule:
                //  - null is invalid operand
                //  - undefined is invalid operand
                if c.both(|(_, ty)| match **ty {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsUndefinedKeyword,
                        ..
                    })
                    | Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsNullKeyword,
                        ..
                    }) => true,

                    _ => false,
                }) {
                    return Err(Error::TS2365 { span });
                }

                if let Some(()) = c.take(|(_, lt), (_, rt)| match lt.normalize() {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsBooleanKeyword,
                        ..
                    }) => match rt.normalize() {
                        Type::Keyword(TsKeywordType {
                            kind: TsKeywordTypeKind::TsNumberKeyword,
                            ..
                        }) => Some(()),
                        _ => None,
                    },
                    _ => None,
                }) {
                    return Ok(Type::Keyword(TsKeywordType {
                        span,
                        kind: TsKeywordTypeKind::TsBooleanKeyword,
                    })
                    .owned());
                }

                unimplemented!("type_of_bin(+)\nLeft: {:#?}\nRight: {:#?}", lt, rt)
            }
            op!("*") | op!("/") => {
                no_unknown!();

                return Ok(Type::Keyword(TsKeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsNumberKeyword,
                })
                .owned());
            }

            op!(bin, "-")
            | op!("<<")
            | op!(">>")
            | op!(">>>")
            | op!("%")
            | op!("|")
            | op!("&")
            | op!("^")
            | op!("**") => {
                no_unknown!();

                return Ok(Type::Keyword(TsKeywordType {
                    kind: TsKeywordTypeKind::TsNumberKeyword,
                    span,
                })
                .into_cow());
            }

            op!("===") | op!("!==") | op!("!=") | op!("==") => {
                return Ok(Type::Keyword(TsKeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsBooleanKeyword,
                })
                .owned());
            }

            op!("<=") | op!("<") | op!(">=") | op!(">") | op!("in") | op!("instanceof") => {
                no_unknown!();

                return Ok(Type::Keyword(TsKeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsBooleanKeyword,
                })
                .owned());
            }

            op!("||") | op!("&&") => {
                no_unknown!();

                match lt.normalize() {
                    Type::Keyword(TsKeywordType {
                        kind: TsKeywordTypeKind::TsAnyKeyword,
                        ..
                    }) => return Ok(Type::any(span).owned()),

                    _ => {}
                }

                return Ok(rt);
            }

            op!("??") => unimplemented!("type_of_bin_expr (`??`)"),
        }
    }
}