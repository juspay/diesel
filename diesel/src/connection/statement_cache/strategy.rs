use crate::backend::Backend;
use crate::util::std_compat::Entry;
use crate::util::std_compat::HashMap;
use core::hash::Hash;

use super::{CacheSize, StatementCacheKey};

/// Indicates the cache key status
//
// This is a separate enum and not just `Option<Entry>`
// as we need to return the cache key for owner ship reasons
// if we don't have a cache at all
#[cfg_attr(
    feature = "i-implement-a-third-party-backend-and-opt-into-breaking-changes",
    allow(missing_debug_implementations)
)]
// cannot implement debug easily as StatementCacheKey is not Debug
pub enum LookupStatementResult<'a, DB, Statement>
where
    DB: Backend,
{
    /// The cache entry, either already populated or vacant
    /// in the later case the caller needs to prepare the
    /// statement and insert it into the cache
    CacheEntry(Entry<'a, StatementCacheKey<DB>, Statement>),
    /// This key should not be cached
    NoCache(StatementCacheKey<DB>),
}

/// Implement this trait, in order to control statement caching.
#[allow(unreachable_pub)]
pub trait StatementCacheStrategy<DB, Statement>: Send + 'static
where
    DB: Backend,
    StatementCacheKey<DB>: Hash + Eq,
{
    /// Returns which prepared statement cache size is implemented by this trait
    fn cache_size(&self) -> CacheSize;

    /// Returns whether or not the corresponding cache key is already cached
    fn lookup_statement(
        &mut self,
        key: StatementCacheKey<DB>,
    ) -> LookupStatementResult<'_, DB, Statement>;

    /// Removes all cached statements so that subsequent queries are re-prepared.
    // Only reached through `StatementCache::clear`, which is currently used only
    // by the SQLite authorizer, so it is dead code for backends compiled without
    // that caller.
    #[cfg(feature = "__sqlite-shared")]
    fn clear(&mut self);
}

/// Cache all (safe) statements for as long as connection is alive.
#[allow(missing_debug_implementations, unreachable_pub)]
pub struct WithCacheStrategy<DB, Statement>
where
    DB: Backend,
{
    cache: HashMap<StatementCacheKey<DB>, Statement>,
}

impl<DB, Statement> Default for WithCacheStrategy<DB, Statement>
where
    DB: Backend,
{
    fn default() -> Self {
        Self {
            cache: Default::default(),
        }
    }
}

impl<DB, Statement> StatementCacheStrategy<DB, Statement> for WithCacheStrategy<DB, Statement>
where
    DB: Backend + 'static,
    StatementCacheKey<DB>: Hash + Eq,
    DB::TypeMetadata: Send + Clone,
    DB::QueryBuilder: Default,
    Statement: Send + 'static,
{
    fn lookup_statement(
        &mut self,
        entry: StatementCacheKey<DB>,
    ) -> LookupStatementResult<'_, DB, Statement> {
        LookupStatementResult::CacheEntry(self.cache.entry(entry))
    }

    fn cache_size(&self) -> CacheSize {
        CacheSize::Unbounded
    }

    #[cfg(feature = "__sqlite-shared")]
    fn clear(&mut self) {
        self.cache.clear();
    }
}

/// No statements will be cached,
#[allow(missing_debug_implementations, unreachable_pub)]
#[derive(Clone, Copy, Default)]
pub struct WithoutCacheStrategy {}

impl<DB, Statement> StatementCacheStrategy<DB, Statement> for WithoutCacheStrategy
where
    DB: Backend,
    StatementCacheKey<DB>: Hash + Eq,
    DB::TypeMetadata: Clone,
    DB::QueryBuilder: Default,
    Statement: 'static,
{
    fn lookup_statement(
        &mut self,
        entry: StatementCacheKey<DB>,
    ) -> LookupStatementResult<'_, DB, Statement> {
        LookupStatementResult::NoCache(entry)
    }

    fn cache_size(&self) -> CacheSize {
        CacheSize::Disabled
    }

    #[cfg(feature = "__sqlite-shared")]
    fn clear(&mut self) {}
}

#[allow(dead_code)]
#[cfg(test)]
mod testing_utils {

    use crate::{
        Connection,
        connection::{Instrumentation, InstrumentationEvent},
    };

    #[derive(Default)]
    pub struct RecordCacheEvents {
        pub list: Vec<String>,
    }

    impl Instrumentation for RecordCacheEvents {
        fn on_connection_event(&mut self, event: InstrumentationEvent<'_>) {
            if let InstrumentationEvent::CacheQuery { sql } = event {
                self.list.push(sql.to_owned());
            }
        }
    }

    pub fn count_cache_calls(conn: &mut impl Connection) -> usize {
        if let Some(events) = conn
            .instrumentation()
            .as_any()
            .downcast_ref::<RecordCacheEvents>()
        {
            events.list.len()
        } else {
            0
        }
    }
}

#[cfg(test)]
#[cfg(feature = "postgres")]
mod tests_pg {
    use crate::connection::CacheSize;
    use crate::dsl::sql;
    use crate::insertable::Insertable;
    use crate::pg::Pg;
    use crate::sql_types::{Integer, VarChar};
    use crate::table;
    use crate::test_helpers::pg_database_url;
    use crate::{Connection, ExpressionMethods, IntoSql, PgConnection, QueryDsl, RunQueryDsl};

    use super::testing_utils::{RecordCacheEvents, count_cache_calls};

    #[crate::declare_sql_function]
    extern "SQL" {
        fn lower(x: VarChar) -> VarChar;
    }

    table! {
        users {
            id -> Integer,
            name -> Text,
        }
    }

    pub fn connection() -> PgConnection {
        let mut conn = PgConnection::establish(&pg_database_url()).unwrap();
        conn.set_instrumentation(RecordCacheEvents::default());
        conn
    }

    #[diesel_test_helper::test]
    fn prepared_statements_are_cached() {
        let connection = &mut connection();

        let query = crate::select(1.into_sql::<Integer>());

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_with_identical_sql_but_different_types_are_cached_separately() {
        let connection = &mut connection();

        let query = crate::select(1.into_sql::<Integer>());
        let query2 = crate::select("hi".into_sql::<VarChar>());

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
        assert_eq!(Ok("hi".to_string()), query2.get_result(connection));
        assert_eq!(2, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_with_identical_types_and_sql_but_different_bind_types_are_cached_separately() {
        let connection = &mut connection();

        let query = crate::select(1.into_sql::<Integer>()).into_boxed::<Pg>();
        let query2 = crate::select("hi".into_sql::<VarChar>()).into_boxed::<Pg>();

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
        assert_eq!(Ok("hi".to_string()), query2.get_result(connection));
        assert_eq!(2, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_with_identical_types_and_binds_but_different_sql_are_cached_separately() {
        let connection = &mut connection();

        let hi = "HI".into_sql::<VarChar>();
        let query = crate::select(hi).into_boxed::<Pg>();
        let query2 = crate::select(lower(hi)).into_boxed::<Pg>();

        assert_eq!(Ok("HI".to_string()), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
        assert_eq!(Ok("hi".to_string()), query2.get_result(connection));
        assert_eq!(2, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_with_sql_literal_nodes_are_not_cached() {
        let connection = &mut connection();
        let query = crate::select(sql::<Integer>("1"));

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn inserts_from_select_are_cached() {
        let connection = &mut connection();
        connection.begin_test_transaction().unwrap();

        crate::sql_query(
            "CREATE TEMPORARY TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .execute(connection)
        .unwrap();

        let query = users::table.filter(users::id.eq(42));
        let insert = query
            .insert_into(users::table)
            .into_columns((users::id, users::name));
        assert!(insert.execute(connection).is_ok());
        assert_eq!(1, count_cache_calls(connection));

        let query = users::table.filter(users::id.eq(42)).into_boxed();
        let insert = query
            .insert_into(users::table)
            .into_columns((users::id, users::name));
        assert!(insert.execute(connection).is_ok());
        assert_eq!(2, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn single_inserts_are_cached() {
        let connection = &mut connection();
        connection.begin_test_transaction().unwrap();

        crate::sql_query(
            "CREATE TEMPORARY TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .execute(connection)
        .unwrap();

        let insert =
            crate::insert_into(users::table).values((users::id.eq(42), users::name.eq("Foo")));

        assert!(insert.execute(connection).is_ok());
        assert_eq!(1, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn dynamic_batch_inserts_are_not_cached() {
        let connection = &mut connection();
        connection.begin_test_transaction().unwrap();

        crate::sql_query(
            "CREATE TEMPORARY TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .execute(connection)
        .unwrap();

        let insert = crate::insert_into(users::table)
            .values(vec![(users::id.eq(42), users::name.eq("Foo"))]);

        assert!(insert.execute(connection).is_ok());
        assert_eq!(0, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn static_batch_inserts_are_cached() {
        let connection = &mut connection();
        connection.begin_test_transaction().unwrap();

        crate::sql_query(
            "CREATE TEMPORARY TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .execute(connection)
        .unwrap();

        let insert =
            crate::insert_into(users::table).values([(users::id.eq(42), users::name.eq("Foo"))]);

        assert!(insert.execute(connection).is_ok());
        assert_eq!(1, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_containing_in_with_vec_are_cached() {
        let connection = &mut connection();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = crate::select(one_as_expr.eq_any(vec![1, 2, 3]));

        assert_eq!(Ok(true), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn disabling_the_cache_works() {
        let connection = &mut connection();
        connection.set_prepared_statement_cache_size(CacheSize::Disabled);

        let query = crate::select(1.into_sql::<Integer>());

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
    }
}

#[cfg(test)]
#[cfg(feature = "__sqlite-shared")]
mod tests_sqlite {

    use crate::connection::{CacheSize, SimpleConnection};
    use crate::dsl::sql;
    use crate::query_dsl::RunQueryDsl;
    use crate::sql_types::Integer;
    use crate::{Connection, ExpressionMethods, IntoSql, QueryDsl, SqliteConnection};

    use super::testing_utils::{RecordCacheEvents, count_cache_calls};

    crate::table! {
        users {
            id -> Integer,
        }
    }

    pub fn connection() -> SqliteConnection {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.set_instrumentation(RecordCacheEvents::default());
        conn
    }

    /// Two attached schemas, each with an empty `users` table.
    fn connection_with_two_schemas() -> SqliteConnection {
        let mut conn = connection();
        conn.batch_execute(
            "ATTACH DATABASE ':memory:' AS tenant_a; \
             ATTACH DATABASE ':memory:' AS tenant_b; \
             CREATE TABLE tenant_a.users (id INTEGER PRIMARY KEY); \
             CREATE TABLE tenant_b.users (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        conn
    }

    /// Rows present in `<schema>.users`. Reads via the schema-qualified SELECT
    /// path, whose per-schema correctness is established by
    /// `schema_qualified_queries_are_cached_per_schema`.
    fn ids_in(conn: &mut SqliteConnection, schema: &str) -> Vec<i32> {
        let schema = String::from(schema);
        users::table
            .select(users::id)
            .schema_name(&schema)
            .load(conn)
            .unwrap()
    }

    /// The same query against two different schemas must not share a cached
    /// prepared statement: the schema name is only known at runtime, so it
    /// cannot take part in the type-level query id.
    #[diesel_test_helper::test]
    fn schema_qualified_queries_are_cached_per_schema() {
        let connection = &mut connection();
        // Two real schemas, each with a `users` table holding a distinguishable row.
        connection
            .batch_execute(
                "ATTACH DATABASE ':memory:' AS tenant_a; \
                 ATTACH DATABASE ':memory:' AS tenant_b; \
                 CREATE TABLE tenant_a.users (id INTEGER PRIMARY KEY); \
                 CREATE TABLE tenant_b.users (id INTEGER PRIMARY KEY); \
                 INSERT INTO tenant_a.users (id) VALUES (1); \
                 INSERT INTO tenant_b.users (id) VALUES (2);",
            )
            .unwrap();

        let schema_a = String::from("tenant_a");
        let schema_b = String::from("tenant_b");

        assert_eq!(
            Ok(1),
            users::table
                .select(users::id)
                .schema_name(&schema_a)
                .get_result(connection)
        );
        assert_eq!(1, count_cache_calls(connection));

        // Same type, different schema: must re-prepare rather than reuse
        // `tenant_a`'s statement and silently read the wrong schema.
        assert_eq!(
            Ok(2),
            users::table
                .select(users::id)
                .schema_name(&schema_b)
                .get_result(connection)
        );
        assert_eq!(2, count_cache_calls(connection));

        // Repeating a schema still hits the cache rather than growing it.
        assert_eq!(
            Ok(1),
            users::table
                .select(users::id)
                .schema_name(&schema_a)
                .get_result(connection)
        );
        assert_eq!(2, count_cache_calls(connection));
    }

    /// An INSERT must land in the schema it names, not in whichever schema was
    /// inserted into first on this connection.
    #[diesel_test_helper::test]
    fn schema_qualified_inserts_are_cached_per_schema() {
        let connection = &mut connection_with_two_schemas();
        let schema_a = String::from("tenant_a");
        let schema_b = String::from("tenant_b");

        crate::insert_into(users::table)
            .values(users::id.eq(1))
            .schema_name(&schema_a)
            .execute(connection)
            .unwrap();
        assert_eq!(1, count_cache_calls(connection));

        crate::insert_into(users::table)
            .values(users::id.eq(2))
            .schema_name(&schema_b)
            .execute(connection)
            .unwrap();
        assert_eq!(2, count_cache_calls(connection));

        // Each row landed in its own schema.
        assert_eq!(vec![1], ids_in(connection, "tenant_a"));
        assert_eq!(vec![2], ids_in(connection, "tenant_b"));
    }

    /// A DELETE must remove rows from the schema it names, not from whichever
    /// schema was deleted from first on this connection.
    #[diesel_test_helper::test]
    fn schema_qualified_deletes_are_cached_per_schema() {
        let connection = &mut connection_with_two_schemas();
        connection
            .batch_execute(
                "INSERT INTO tenant_a.users (id) VALUES (1); \
                 INSERT INTO tenant_b.users (id) VALUES (2);",
            )
            .unwrap();
        let schema_a = String::from("tenant_a");
        let schema_b = String::from("tenant_b");

        assert_eq!(
            Ok(1),
            crate::delete(users::table)
                .schema_name(&schema_a)
                .execute(connection)
        );
        // tenant_b must be untouched by tenant_a's delete.
        assert!(ids_in(connection, "tenant_a").is_empty());
        assert_eq!(vec![2], ids_in(connection, "tenant_b"));

        assert_eq!(
            Ok(1),
            crate::delete(users::table)
                .schema_name(&schema_b)
                .execute(connection)
        );
        assert!(ids_in(connection, "tenant_b").is_empty());
    }

    /// Queries that never call `.schema_name()` must still be cached and
    /// reused. They no longer have a static query id, so they are keyed on
    /// their SQL instead, but a repeated query must not re-prepare.
    #[diesel_test_helper::test]
    fn plain_table_queries_are_still_cached_and_reused() {
        let connection = &mut connection();
        connection
            .batch_execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY); \
                 INSERT INTO users (id) VALUES (7);",
            )
            .unwrap();

        assert_eq!(Ok(7), users::table.select(users::id).get_result(connection));
        assert_eq!(1, count_cache_calls(connection));

        // Second run must hit the cache rather than prepare a second statement.
        assert_eq!(Ok(7), users::table.select(users::id).get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
    }

    /// Bind values are placeholders in the SQL, so queries differing only in
    /// bind values must share one cached statement rather than growing the
    /// cache per value.
    #[diesel_test_helper::test]
    fn plain_queries_with_different_bind_values_share_one_cached_statement() {
        let connection = &mut connection();
        connection
            .batch_execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY); \
                 INSERT INTO users (id) VALUES (1), (2);",
            )
            .unwrap();

        assert_eq!(
            Ok(1),
            users::table
                .select(users::id)
                .filter(users::id.eq(1))
                .get_result(connection)
        );
        assert_eq!(1, count_cache_calls(connection));

        assert_eq!(
            Ok(2),
            users::table
                .select(users::id)
                .filter(users::id.eq(2))
                .get_result(connection)
        );
        assert_eq!(1, count_cache_calls(connection));
    }

    /// An unqualified query and a schema-qualified one target different tables
    /// and must not share a cached statement in either direction.
    #[diesel_test_helper::test]
    fn unqualified_and_schema_qualified_queries_do_not_share_a_statement() {
        let connection = &mut connection_with_two_schemas();
        connection
            .batch_execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY); \
                 INSERT INTO users (id) VALUES (9); \
                 INSERT INTO tenant_a.users (id) VALUES (1);",
            )
            .unwrap();
        let schema_a = String::from("tenant_a");

        // Unqualified reads the default schema.
        assert_eq!(Ok(9), users::table.select(users::id).get_result(connection));
        assert_eq!(1, count_cache_calls(connection));

        // Qualified reads tenant_a, and does not reuse the unqualified statement.
        assert_eq!(
            Ok(1),
            users::table
                .select(users::id)
                .schema_name(&schema_a)
                .get_result(connection)
        );
        assert_eq!(2, count_cache_calls(connection));

        // ...and the unqualified query still reads the default schema afterwards.
        assert_eq!(Ok(9), users::table.select(users::id).get_result(connection));
        assert_eq!(2, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn prepared_statements_are_cached_when_run() {
        let connection = &mut connection();
        let query = crate::select(1.into_sql::<Integer>());

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn sql_literal_nodes_are_not_cached() {
        let connection = &mut connection();
        let query = crate::select(sql::<Integer>("1"));

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_containing_sql_literal_nodes_are_not_cached() {
        let connection = &mut connection();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = crate::select(one_as_expr.eq(sql::<Integer>("1")));

        assert_eq!(Ok(true), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_containing_in_with_vec_are_not_cached() {
        let connection = &mut connection();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = crate::select(one_as_expr.eq_any(vec![1, 2, 3]));

        assert_eq!(Ok(true), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn queries_containing_in_with_subselect_are_cached() {
        let connection = &mut connection();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = crate::select(one_as_expr.eq_any(crate::select(one_as_expr)));

        assert_eq!(Ok(true), query.get_result(connection));
        assert_eq!(1, count_cache_calls(connection));
    }

    #[diesel_test_helper::test]
    fn disabling_the_cache_works() {
        let connection = &mut connection();
        connection.set_prepared_statement_cache_size(CacheSize::Disabled);

        let query = crate::select(1.into_sql::<Integer>());

        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
        assert_eq!(Ok(1), query.get_result(connection));
        assert_eq!(0, count_cache_calls(connection));
    }
}
