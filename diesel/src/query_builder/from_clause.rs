use super::{AstPass, QueryFragment, QueryId};
use crate::backend::Backend;
use crate::query_source::AppearsInFromClause;
use crate::{QueryResult, QuerySource};
use alloc::string::{String, ToString};

/// This type represents a not existing from clause
///
/// Custom backends can provide a custom [`QueryFragment`]
/// impl by specializing the implementation via
/// [`SqlDialect::EmptyFromClauseSyntax`](crate::backend::SqlDialect::EmptyFromClauseSyntax)
#[cfg_attr(
    feature = "i-implement-a-third-party-backend-and-opt-into-breaking-changes",
    cfg(feature = "i-implement-a-third-party-backend-and-opt-into-breaking-changes")
)]
#[derive(Debug, Clone, Copy, QueryId)]
pub struct NoFromClause;

impl<DB> QueryFragment<DB> for NoFromClause
where
    Self: QueryFragment<DB, DB::EmptyFromClauseSyntax>,
    DB: Backend,
{
    fn walk_ast<'b>(&'b self, pass: AstPass<'_, 'b, DB>) -> QueryResult<()> {
        <Self as QueryFragment<DB, DB::EmptyFromClauseSyntax>>::walk_ast(self, pass)
    }
}

impl<DB> QueryFragment<DB, crate::backend::sql_dialect::from_clause_syntax::AnsiSqlFromClauseSyntax>
    for NoFromClause
where
    DB: Backend<
        EmptyFromClauseSyntax = crate::backend::sql_dialect::from_clause_syntax::AnsiSqlFromClauseSyntax,
    >,
{
    fn walk_ast<'b>(&'b self, _pass: AstPass<'_, 'b, DB>) -> QueryResult<()> {
        Ok(())
    }
}

pub trait AsQuerySource {
    type QuerySource: QuerySource;
    type FromClause;

    fn as_query_source(&self) -> &Self::QuerySource;
    #[allow(clippy::wrong_self_convention)] // `from` refers to the SQL clause
    fn from_clause(&self) -> Self::FromClause;
}

impl<QS> AsQuerySource for QS
where
    QS: QuerySource,
{
    type QuerySource = Self;
    type FromClause = QS::FromClause;

    fn as_query_source(&self) -> &Self::QuerySource {
        self
    }

    fn from_clause(&self) -> Self::FromClause {
        QuerySource::from_clause(self)
    }
}

#[doc(hidden)]
pub trait SchemaName {
    const HAS_STATIC_QUERY_ID: bool = false;

    type FromClause<T>;

    fn as_schema_name(&self) -> Option<&str>;
    fn qualify_from_clause<T>(&self, from_clause: T) -> Self::FromClause<T>;
}

impl SchemaName for () {
    const HAS_STATIC_QUERY_ID: bool = true;

    type FromClause<T> = T;

    fn as_schema_name(&self) -> Option<&str> {
        None
    }

    fn qualify_from_clause<T>(&self, from_clause: T) -> Self::FromClause<T> {
        from_clause
    }
}

impl SchemaName for String {
    type FromClause<T> = SchemaQualifiedFromClause<T>;

    fn as_schema_name(&self) -> Option<&str> {
        Some(self)
    }

    fn qualify_from_clause<T>(&self, from_clause: T) -> Self::FromClause<T> {
        SchemaQualifiedFromClause {
            from_clause,
            schema_name: self.clone(),
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct SchemaQualifiedFromClause<T> {
    from_clause: T,
    schema_name: String,
}

impl<DB, T> QueryFragment<DB> for SchemaQualifiedFromClause<T>
where
    DB: Backend,
    T: QueryFragment<DB>,
{
    fn walk_ast<'b>(&'b self, pass: AstPass<'_, 'b, DB>) -> QueryResult<()> {
        self.from_clause
            .walk_ast_with_schema(&self.schema_name, pass)
    }

    fn walk_ast_with_schema<'b>(
        &'b self,
        _schema: &str,
        pass: AstPass<'_, 'b, DB>,
    ) -> QueryResult<()> {
        // Preserve independently qualified relations inside a schema-qualified join.
        self.walk_ast(pass)
    }
}

#[doc(hidden)]
pub struct FromClause<F: QuerySource, S = ()> {
    pub(crate) source: F,
    pub(crate) schema_name: S,
    pub(crate) from_clause: F::FromClause,
}

impl<F, S> AsQuerySource for FromClause<F, S>
where
    F: QuerySource,
    S: SchemaName,
{
    type QuerySource = F;
    type FromClause = S::FromClause<F::FromClause>;

    fn as_query_source(&self) -> &Self::QuerySource {
        &self.source
    }

    fn from_clause(&self) -> Self::FromClause {
        self.schema_name
            .qualify_from_clause(self.source.from_clause())
    }
}

impl<F, S> QueryId for FromClause<F, S>
where
    F: QuerySource + QueryId,
    S: SchemaName,
{
    type QueryId = F::QueryId;

    const HAS_STATIC_QUERY_ID: bool = F::HAS_STATIC_QUERY_ID && S::HAS_STATIC_QUERY_ID;
}

impl<F, S> Copy for FromClause<F, S>
where
    F: QuerySource + Copy,
    F::FromClause: Copy,
    S: Copy,
{
}

impl<F, S> Clone for FromClause<F, S>
where
    F: QuerySource + Clone,
    F::FromClause: Clone,
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            schema_name: self.schema_name.clone(),
            from_clause: self.from_clause.clone(),
        }
    }
}

impl<F, S> core::fmt::Debug for FromClause<F, S>
where
    F: QuerySource,
    F::FromClause: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FromClause")
            .field("from_clause", &self.from_clause)
            .finish()
    }
}

impl<F: QuerySource> FromClause<F> {
    pub(crate) fn new(qs: F) -> Self {
        Self {
            from_clause: qs.from_clause(),
            schema_name: (),
            source: qs,
        }
    }
}

impl<F: QuerySource, S> FromClause<F, S> {
    pub(crate) fn set_schema_name(self, schema_name: &str) -> FromClause<F, String> {
        FromClause {
            from_clause: self.from_clause,
            schema_name: schema_name.to_string(),
            source: self.source,
        }
    }

    pub(crate) fn map_source<NewF>(self, f: impl FnOnce(F) -> NewF) -> FromClause<NewF, S>
    where
        NewF: QuerySource,
    {
        let source = f(self.source);
        FromClause {
            from_clause: source.from_clause(),
            schema_name: self.schema_name,
            source,
        }
    }
}

impl<DB, F, S> QueryFragment<DB> for FromClause<F, S>
where
    F: QuerySource,
    DB: Backend,
    F::FromClause: QueryFragment<DB>,
    S: SchemaName,
{
    fn walk_ast<'b>(&'b self, mut pass: AstPass<'_, 'b, DB>) -> QueryResult<()> {
        pass.push_sql(" FROM ");
        if let Some(schema) = self.schema_name.as_schema_name() {
            self.from_clause.walk_ast_with_schema(schema, pass)
        } else {
            self.from_clause.walk_ast(pass)
        }
    }
}

impl<QS1, QS2, S> AppearsInFromClause<QS1> for FromClause<QS2, S>
where
    QS1: QuerySource,
    QS2: QuerySource,
    QS2: AppearsInFromClause<QS1>,
{
    type Count = <QS2 as AppearsInFromClause<QS1>>::Count;
}
