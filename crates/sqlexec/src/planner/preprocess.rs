//! AST visitors for preprocessing queries before planning.
use std::ops::ControlFlow;

use datafusion::sql::sqlparser::ast::{self, VisitMut, VisitorMut};
use sqlbuiltins::builtins::DEFAULT_CATALOG;

use crate::context::local::LocalSessionContext;

#[derive(Debug, thiserror::Error)]
pub enum PreprocessError {
    #[error("Relation '{0}' does not exist")]
    MissingRelation(String),

    #[error("Casting expressions to regclass unsupported")]
    ExprUnsupportedRegclassCast,

    #[error("Casting expressions to oid unsupported")]
    ExprUnsupportedOIDCast,
}

pub fn preprocess<V>(statement: &mut ast::Statement, visitor: &mut V) -> Result<(), PreprocessError>
where
    V: VisitorMut<Break = PreprocessError>,
{
    match statement.visit(visitor) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(e) => Err(e),
    }
}

/// Replace `CAST('table_name' as [REGCLASS | OID])` expressions with the oid of the
/// table.
pub struct CastOIDReplacer<'a> {
    pub ctx: &'a LocalSessionContext,
}

impl<'a> ast::VisitorMut for CastOIDReplacer<'a> {
    type Break = PreprocessError;

    fn post_visit_expr(&mut self, expr: &mut ast::Expr) -> ControlFlow<Self::Break> {
        fn find_oid(ctx: &LocalSessionContext, rel: &str) -> Option<u32> {
            let catalog = ctx.get_session_catalog();
            for schema in ctx.implicit_search_paths() {
                // TODO
                if let Some(ent) = catalog.resolve_entry(DEFAULT_CATALOG, &schema, rel) {
                    // Table found.
                    return Some(ent.get_meta().id);
                }
            }
            None
        }

        let replace_expr = match expr {
            ast::Expr::Cast {
                expr: inner_expr,
                data_type,
                format: _,
            } => {
                match data_type {
                    ast::DataType::Regclass => {}
                    ast::DataType::Custom(name, _) if name.to_string().to_lowercase() == "oid" => {}
                    _ => return ControlFlow::Continue(()), // Nothing to do.
                }
                if let ast::Expr::Value(ast::Value::SingleQuotedString(relation)) = &**inner_expr {
                    match find_oid(self.ctx, relation) {
                        Some(oid) => ast::Expr::Value(ast::Value::Number(oid.to_string(), false)),
                        None => {
                            return ControlFlow::Break(PreprocessError::MissingRelation(
                                relation.clone(),
                            ))
                        }
                    }
                } else {
                    // We don't currently support any other casts to regclass or oid.
                    let e = match data_type {
                        ast::DataType::Regclass => PreprocessError::ExprUnsupportedRegclassCast,
                        ast::DataType::Custom(_, _) => PreprocessError::ExprUnsupportedOIDCast,
                        _ => unreachable!(),
                    };
                    return ControlFlow::Break(e);
                }
            }
            _ => return ControlFlow::Continue(()), // Nothing to do.
        };

        *expr = replace_expr;

        ControlFlow::Continue(())
    }
}

/// Replace `E'my_string'` with `"my_string"`.
///
/// TODO: Datafusion should be updated to properly handle escaped strings. This
/// is just a quick hack.
pub struct EscapedStringToDoubleQuoted;

impl ast::VisitorMut for EscapedStringToDoubleQuoted {
    type Break = PreprocessError;

    fn post_visit_expr(&mut self, expr: &mut ast::Expr) -> ControlFlow<Self::Break> {
        if let ast::Expr::Value(ast::Value::EscapedStringLiteral(s)) = expr {
            *expr = ast::Expr::Value(ast::Value::DoubleQuotedString(std::mem::take(s)));
        }
        ControlFlow::Continue(())
    }
}
