use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use datafusion::arrow::datatypes::{
    DataType,
    Field,
    Schema,
    TimeUnit,
    DECIMAL128_MAX_PRECISION,
    DECIMAL_DEFAULT_SCALE,
};
use datafusion::common::parsers::CompressionTypeVariant;
use datafusion::common::{OwnedSchemaReference, OwnedTableReference, ToDFSchema};
use datafusion::logical_expr::{cast, col, LogicalPlanBuilder};
use datafusion::sql::planner::{object_name_to_table_reference, PlannerContext};
use datafusion::sql::TableReference;
use datafusion_ext::conversion::convert;
use datafusion_ext::planner::SqlQueryPlanner;
use datafusion_ext::{AsyncContextProvider, IdentNormalizer};
use datasources::bigquery::{BigQueryAccessor, BigQueryTableAccess};
use datasources::cassandra::{CassandraAccess, CassandraAccessState};
use datasources::clickhouse::{ClickhouseAccess, ClickhouseTableRef};
use datasources::common::ssh::key::SshKey;
use datasources::common::ssh::{SshConnection, SshConnectionParameters};
use datasources::common::url::{DatasourceUrl, DatasourceUrlType};
use datasources::debug::DebugTableType;
use datasources::lake::delta::access::{load_table_direct, DeltaLakeAccessor};
use datasources::lake::iceberg::table::IcebergTable;
use datasources::lake::storage_options_into_object_store;
use datasources::lance::LanceTable;
use datasources::mongodb::{MongoDbAccessor, MongoDbConnection};
use datasources::mysql::{MysqlAccessor, MysqlDbConnection, MysqlTableAccess};
use datasources::object_store::azure::AzureStoreAccess;
use datasources::object_store::gcs::GcsStoreAccess;
use datasources::object_store::local::LocalStoreAccess;
use datasources::object_store::s3::S3StoreAccess;
use datasources::object_store::{file_type_from_path, ObjStoreAccess, ObjStoreAccessor};
use datasources::postgres::{PostgresAccess, PostgresDbConnection};
use datasources::snowflake::{SnowflakeAccessor, SnowflakeDbConnection, SnowflakeTableAccess};
use datasources::sqlserver::SqlServerAccess;
use object_store::aws::AmazonS3ConfigKey;
use object_store::azure::AzureConfigKey;
use object_store::gcp::GoogleConfigKey;
use parser::options::StatementOptions;
use parser::sqlparser::ast::{
    self,
    ColumnOption,
    DescribeAlias,
    FromTable,
    Ident,
    ObjectName,
    ObjectType,
};
use parser::{
    self,
    validate_ident,
    validate_object_name,
    AlterDatabaseStmt,
    AlterTableStmtExtension,
    AlterTunnelAction,
    AlterTunnelStmt,
    CopyToSource,
    CopyToStmt,
    CreateCredentialStmt,
    CreateCredentialsStmt,
    CreateExternalDatabaseStmt,
    CreateExternalTableStmt,
    CreateTunnelStmt,
    DropCredentialsStmt,
    DropDatabaseStmt,
    DropTunnelStmt,
    StatementWithExtensions,
};
use protogen::metastore::types::catalog::{
    CatalogEntry,
    DatabaseEntry,
    RuntimePreference,
    SourceAccessMode,
    TableEntry,
};
use protogen::metastore::types::options::{
    CopyToDestinationOptions,
    CopyToDestinationOptionsAzure,
    CopyToDestinationOptionsGcs,
    CopyToDestinationOptionsLocal,
    CopyToDestinationOptionsS3,
    CopyToFormatOptions,
    CopyToFormatOptionsBson,
    CopyToFormatOptionsCsv,
    CopyToFormatOptionsDelta,
    CopyToFormatOptionsJson,
    CopyToFormatOptionsLance,
    CopyToFormatOptionsParquet,
    CredentialsOptions,
    CredentialsOptionsAws,
    CredentialsOptionsAzure,
    CredentialsOptionsDebug,
    CredentialsOptionsGcp,
    CredentialsOptionsOpenAI,
    DatabaseOptions,
    DatabaseOptionsBigQuery,
    DatabaseOptionsCassandra,
    DatabaseOptionsClickhouse,
    DatabaseOptionsDebug,
    DatabaseOptionsDeltaLake,
    DatabaseOptionsMongoDb,
    DatabaseOptionsMysql,
    DatabaseOptionsPostgres,
    DatabaseOptionsSnowflake,
    DatabaseOptionsSqlServer,
    DatabaseOptionsSqlite,
    DeltaLakeCatalog,
    DeltaLakeUnityCatalog,
    StorageOptions,
    TableOptionsBigQuery,
    TableOptionsCassandra,
    TableOptionsClickhouse,
    TableOptionsDebug,
    TableOptionsExcel,
    TableOptionsGcs,
    TableOptionsLocal,
    TableOptionsMongoDb,
    TableOptionsMysql,
    TableOptionsObjectStore,
    TableOptionsPostgres,
    TableOptionsS3,
    TableOptionsSnowflake,
    TableOptionsSqlServer,
    TableOptionsV0,
    TunnelOptions,
    TunnelOptionsDebug,
    TunnelOptionsInternal,
    TunnelOptionsSsh,
};
use protogen::metastore::types::service::{AlterDatabaseOperation, AlterTableOperation};
use sqlbuiltins::builtins::{CURRENT_SESSION_SCHEMA, DEFAULT_CATALOG};
use sqlbuiltins::validation::{
    validate_copyto_dest_creds_support,
    validate_copyto_dest_format_support,
    validate_database_creds_support,
    validate_database_tunnel_support,
    validate_table_creds_support,
    validate_table_tunnel_support,
};
use tracing::debug;

use super::context_builder::PartialContextProvider;
use super::extension::ExtensionNode;
use super::physical_plan::remote_scan::ProviderReference;
use crate::context::local::LocalSessionContext;
use crate::planner::errors::{internal, PlanError, Result};
use crate::planner::logical_plan::{
    AlterDatabase,
    AlterTable,
    AlterTunnelRotateKeys,
    CopyTo,
    CreateCredentials,
    CreateExternalDatabase,
    CreateExternalTable,
    CreateSchema,
    CreateTable,
    CreateTempTable,
    CreateTunnel,
    CreateView,
    Delete,
    DescribeTable,
    DropCredentials,
    DropDatabase,
    DropSchemas,
    DropTables,
    DropTunnel,
    DropViews,
    FullObjectReference,
    Insert,
    LogicalPlan,
    SetVariable,
    ShowVariable,
    TransactionPlan,
    Update,
};
use crate::planner::preprocess::{preprocess, CastOIDReplacer, EscapedStringToDoubleQuoted};
use crate::remote::table::StubRemoteTableProvider;
use crate::resolve::{EntryResolver, ResolvedEntry};

/// Plan SQL statements for a session.
pub struct SessionPlanner<'a> {
    ctx: &'a LocalSessionContext,
}

struct PlanCredentialArgs {
    /// Name of the credentials as it exists in GlareDB.
    name: Ident,
    /// The credentials provider.
    provider: Ident,
    /// Credentials specific options.
    options: StatementOptions,
    /// Optional comment (what the credentials are for).
    comment: String,
    or_replace: bool,
}

impl From<CreateCredentialsStmt> for PlanCredentialArgs {
    fn from(value: CreateCredentialsStmt) -> Self {
        PlanCredentialArgs {
            name: value.name,
            provider: value.provider,
            options: value.options,
            comment: value.comment,
            or_replace: value.or_replace,
        }
    }
}

impl From<CreateCredentialStmt> for PlanCredentialArgs {
    fn from(value: CreateCredentialStmt) -> Self {
        PlanCredentialArgs {
            name: value.name,
            provider: value.provider,
            options: value.options,
            comment: value.comment,
            or_replace: value.or_replace,
        }
    }
}

impl<'a> SessionPlanner<'a> {
    pub fn new(ctx: &'a LocalSessionContext) -> Self {
        SessionPlanner { ctx }
    }

    pub async fn plan_ast(&self, mut statement: StatementWithExtensions) -> Result<LogicalPlan> {
        debug!(%statement, "planning sql statement");

        // Run replacers as needed.
        if let StatementWithExtensions::Statement(inner) = &mut statement {
            preprocess(inner, &mut CastOIDReplacer { ctx: self.ctx })?;
            preprocess(inner, &mut EscapedStringToDoubleQuoted)?;
        }

        match statement {
            StatementWithExtensions::Statement(stmt) => self.plan_statement(stmt).await,
            StatementWithExtensions::CreateExternalTable(stmt) => {
                self.plan_create_external_table(stmt).await
            }
            StatementWithExtensions::CreateExternalDatabase(stmt) => {
                self.plan_create_external_database(stmt).await
            }
            StatementWithExtensions::DropDatabase(stmt) => self.plan_drop_database(stmt),
            StatementWithExtensions::AlterDatabase(stmt) => self.plan_alter_database(stmt),
            StatementWithExtensions::AlterTableExtension(stmt) => {
                self.plan_alter_table_extension(stmt)
            }
            StatementWithExtensions::CreateTunnel(stmt) => self.plan_create_tunnel(stmt),
            StatementWithExtensions::DropTunnel(stmt) => self.plan_drop_tunnel(stmt),
            StatementWithExtensions::AlterTunnel(stmt) => self.plan_alter_tunnel(stmt),
            StatementWithExtensions::CreateCredentials(stmt) => {
                self.plan_create_credentials(stmt.into())
            }
            StatementWithExtensions::DropCredentials(stmt) => self.plan_drop_credentials(stmt),
            StatementWithExtensions::CopyTo(stmt) => self.plan_copy_to(stmt).await,
        }
    }

    async fn plan_create_external_database(
        &self,
        mut stmt: CreateExternalDatabaseStmt,
    ) -> Result<LogicalPlan> {
        let datasource = normalize_ident(stmt.datasource);

        let tunnel = stmt.tunnel.map(normalize_ident);
        let tunnel_options = self.get_tunnel_opts(&tunnel)?;
        if let Some(tunnel_options) = &tunnel_options {
            // Validate if the tunnel type is supported by the datasource
            validate_database_tunnel_support(&datasource, tunnel_options.as_str()).map_err(
                |e| PlanError::InvalidExternalDatabase {
                    source: Box::new(e),
                },
            )?;
        }

        let creds = stmt.credentials.map(normalize_ident);
        let creds_options = self.get_credentials_opts(&creds)?;
        if let Some(creds_options) = &creds_options {
            validate_database_creds_support(&datasource, creds_options.as_str()).map_err(|e| {
                PlanError::InvalidExternalDatabase {
                    source: Box::new(e),
                }
            })?;
        }

        let m = &mut stmt.options;

        let db_options = match datasource.as_str() {
            DatabaseOptions::POSTGRES => {
                let connection_string = get_pg_conn_str(m)?;
                let access =
                    PostgresAccess::new_from_conn_str(connection_string.clone(), tunnel_options);
                access
                    .validate_access()
                    .await
                    .map_err(|e| PlanError::InvalidExternalDatabase {
                        source: Box::new(e),
                    })?;
                DatabaseOptions::Postgres(DatabaseOptionsPostgres { connection_string })
            }
            DatabaseOptions::BIGQUERY => {
                let service_account_key = creds_options.as_ref().map(|c| match c {
                    CredentialsOptions::Gcp(c) => c.service_account_key.clone(),
                    other => unreachable!("invalid credentials {other} for bigquery"),
                });

                let service_account_key =
                    m.remove_required_or("service_account_key", service_account_key)?;

                let project_id: String = m.remove_required("project_id")?;

                BigQueryAccessor::validate_external_database(&service_account_key, &project_id)
                    .await
                    .map_err(|e| PlanError::InvalidExternalDatabase {
                        source: Box::new(e),
                    })?;

                DatabaseOptions::BigQuery(DatabaseOptionsBigQuery {
                    service_account_key,
                    project_id,
                })
            }
            DatabaseOptions::MYSQL => {
                let connection_string = get_mysql_conn_str(m)?;
                MysqlAccessor::validate_external_database(&connection_string, tunnel_options)
                    .await
                    .map_err(|e| PlanError::InvalidExternalDatabase {
                        source: Box::new(e),
                    })?;
                DatabaseOptions::Mysql(DatabaseOptionsMysql { connection_string })
            }
            DatabaseOptions::MONGODB => {
                let connection_string = get_mongodb_conn_str(m)?;
                // Validate the accessor
                MongoDbAccessor::validate_external_database(connection_string.as_str())
                    .await
                    .map_err(|e| PlanError::InvalidExternalDatabase {
                        source: Box::new(e),
                    })?;
                DatabaseOptions::MongoDb(DatabaseOptionsMongoDb { connection_string })
            }
            DatabaseOptions::SNOWFLAKE => {
                let account_name: String = m.remove_required("account")?;
                let login_name: String = m.remove_required("username")?;
                let password: String = m.remove_required("password")?;
                let database_name: String = m.remove_required("database")?;
                let warehouse: String = m.remove_required("warehouse")?;
                let role_name: Option<String> = m.remove_optional("role")?;
                SnowflakeAccessor::validate_external_database(SnowflakeDbConnection {
                    account_name: account_name.clone(),
                    login_name: login_name.clone(),
                    password: password.clone(),
                    database_name: database_name.clone(),
                    warehouse: warehouse.clone(),
                    role_name: role_name.clone(),
                })
                .await
                .map_err(|e| PlanError::InvalidExternalDatabase {
                    source: Box::new(e),
                })?;
                DatabaseOptions::Snowflake(DatabaseOptionsSnowflake {
                    account_name,
                    login_name,
                    password,
                    database_name,
                    warehouse,
                    role_name: role_name.unwrap_or_default(),
                })
            }
            DatabaseOptions::DELTA => {
                let catalog = match m.remove_required::<String>("catalog_type")?.as_str() {
                    "unity" => DeltaLakeCatalog::Unity(DeltaLakeUnityCatalog {
                        catalog_id: m.remove_required("catalog_id")?,
                        databricks_access_token: m.remove_required("access_token")?,
                        workspace_url: m.remove_required("workspace_url")?,
                    }),
                    other => return Err(internal!("Unknown catalog type: {}", other)),
                };

                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }

                // Try connecting to validate.
                DeltaLakeAccessor::connect(&catalog, storage_options.clone())
                    .await
                    .map_err(|e| PlanError::InvalidExternalDatabase {
                        source: Box::new(e),
                    })?;

                DatabaseOptions::Delta(DatabaseOptionsDeltaLake {
                    catalog,
                    storage_options,
                })
            }
            DatabaseOptions::SQL_SERVER => {
                let connection_string: String = m.remove_required("connection_string")?;

                // Validate
                let access = SqlServerAccess::try_new_from_ado_string(&connection_string)?;
                access.validate_access().await?;

                DatabaseOptions::SqlServer(DatabaseOptionsSqlServer { connection_string })
            }
            DatabaseOptions::CLICKHOUSE => {
                let connection_string: String = m.remove_required("connection_string")?;

                // Validate
                let access =
                    ClickhouseAccess::new_from_connection_string(connection_string.clone());
                access.validate_access().await?;

                DatabaseOptions::Clickhouse(DatabaseOptionsClickhouse { connection_string })
            }
            DatabaseOptions::CASSANDRA => {
                let host: String = m.remove_required("host")?;
                let username: Option<String> = m.remove_optional("username")?;
                let password: Option<String> = m.remove_optional("password")?;
                let access = CassandraAccess::new(host.clone(), username.clone(), password.clone());
                access.validate_access().await?;

                DatabaseOptions::Cassandra(DatabaseOptionsCassandra {
                    host,
                    username,
                    password,
                })
            }
            DatabaseOptions::SQLITE => {
                let location: String = m.remove_required("location")?;
                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }

                DatabaseOptions::Sqlite(DatabaseOptionsSqlite {
                    location: location.clone(),
                    storage_options: Some(storage_options),
                })
            }
            DatabaseOptions::DEBUG => {
                datasources::debug::validate_tunnel_connections(tunnel_options.as_ref())?;
                DatabaseOptions::Debug(DatabaseOptionsDebug {})
            }
            other => return Err(internal!("unsupported datasource: {}", other)),
        };

        let database_name = normalize_ident(stmt.name);

        let plan = CreateExternalDatabase {
            database_name,
            if_not_exists: stmt.if_not_exists,
            options: db_options,
            tunnel,
        };

        Ok(plan.into_logical_plan())
    }

    /// TODO: This is a temporary implementation.
    /// The datasource should resolve it's own table options
    /// This is mostly for compatibility with the old table options implementation.
    /// Once the datasources are updated to use the new table options, this should be removed.
    async fn get_tbl_opts_from_v0(
        &self,
        datasource: &str,
        m: &mut StatementOptions,
        creds_options: Option<CredentialsOptions>,
        tunnel_options: Option<TunnelOptions>,
    ) -> Result<TableOptionsV0> {
        Ok(match datasource {
            TableOptionsV0::DEBUG => {
                datasources::debug::validate_tunnel_connections(tunnel_options.as_ref())?;

                let typ: Option<DebugTableType> = match creds_options {
                    Some(CredentialsOptions::Debug(c)) => Some(c.table_type.parse()?),
                    Some(other) => unreachable!("invalid credentials {other} for debug datasource"),
                    None => None,
                };
                let table_type: DebugTableType = m.remove_required_or("table_type", typ)?;
                TableOptionsDebug {
                    table_type: table_type.as_str().to_string(),
                }
                .into()
            }
            TableOptionsV0::POSTGRES => {
                let connection_string = get_pg_conn_str(m)?;
                let schema: String = m.remove_required("schema")?;
                let table: String = m.remove_required("table")?;

                let access =
                    PostgresAccess::new_from_conn_str(connection_string.clone(), tunnel_options);
                access
                    .validate_table_access(&schema, &table)
                    .await
                    .map_err(|e| PlanError::InvalidExternalDatabase {
                        source: Box::new(e),
                    })?;

                TableOptionsPostgres {
                    connection_string,
                    schema,
                    table,
                }
                .into()
            }
            TableOptionsV0::BIGQUERY => {
                let service_account_key = creds_options.as_ref().map(|c| match c {
                    CredentialsOptions::Gcp(c) => c.service_account_key.clone(),
                    other => unreachable!("invalid credentials {other} for bigquery"),
                });

                let service_account_key =
                    m.remove_required_or("service_account_key", service_account_key)?;

                let project_id: String = m.remove_required("project_id")?;
                let dataset_id = m.remove_required("dataset_id")?;
                let table_id = m.remove_required("table_id")?;

                let access = BigQueryTableAccess {
                    dataset_id,
                    table_id,
                };

                BigQueryAccessor::validate_table_access(&service_account_key, &project_id, &access)
                    .await
                    .map_err(|e| PlanError::InvalidExternalTable {
                        source: Box::new(e),
                    })?;

                TableOptionsBigQuery {
                    service_account_key,
                    project_id,
                    dataset_id: access.dataset_id,
                    table_id: access.table_id,
                }
                .into()
            }
            TableOptionsV0::MYSQL => {
                let connection_string = get_mysql_conn_str(m)?;
                let schema = m.remove_required("schema")?;
                let table = m.remove_required("table")?;

                let access = MysqlTableAccess {
                    schema,
                    name: table,
                };

                MysqlAccessor::validate_table_access(&connection_string, &access, tunnel_options)
                    .await
                    .map_err(|e| PlanError::InvalidExternalTable {
                        source: Box::new(e),
                    })?;

                TableOptionsMysql {
                    connection_string,
                    schema: access.schema,
                    table: access.name,
                }
                .into()
            }
            TableOptionsV0::MONGODB => {
                let connection_string = get_mongodb_conn_str(m)?;
                let database = m.remove_required("database")?;
                let collection = m.remove_required("collection")?;

                TableOptionsMongoDb {
                    connection_string,
                    database,
                    collection,
                }
                .into()
            }
            TableOptionsV0::SNOWFLAKE => {
                let account_name: String = m.remove_required("account")?;
                let login_name: String = m.remove_required("username")?;
                let password: String = m.remove_required("password")?;
                let database_name: String = m.remove_required("database")?;
                let warehouse: String = m.remove_required("warehouse")?;
                let role_name: Option<String> = m.remove_optional("role")?;
                let schema_name: String = m.remove_required("schema")?;
                let table_name: String = m.remove_required("table")?;

                let conn_params = SnowflakeDbConnection {
                    account_name: account_name.clone(),
                    login_name: login_name.clone(),
                    password: password.clone(),
                    database_name: database_name.clone(),
                    warehouse: warehouse.clone(),
                    role_name: role_name.clone(),
                };

                let access_info = SnowflakeTableAccess {
                    schema_name,
                    table_name,
                };

                let _ = SnowflakeAccessor::validate_table_access(conn_params, &access_info)
                    .await
                    .map_err(|e| PlanError::InvalidExternalTable {
                        source: Box::new(e),
                    })?;

                TableOptionsSnowflake {
                    account_name,
                    login_name,
                    password,
                    database_name,
                    warehouse,
                    role_name: role_name.unwrap_or_default(),
                    schema_name: access_info.schema_name,
                    table_name: access_info.table_name,
                }
                .into()
            }
            TableOptionsV0::SQL_SERVER => {
                let connection_string: String = m.remove_required("connection_string")?;
                let schema_name: String = m.remove_required("schema")?;
                let table_name: String = m.remove_required("table")?;

                // Validate
                let access = SqlServerAccess::try_new_from_ado_string(&connection_string)?;
                access
                    .validate_table_access(&schema_name, &table_name)
                    .await?;

                TableOptionsSqlServer {
                    connection_string,
                    schema: schema_name,
                    table: table_name,
                }
                .into()
            }
            TableOptionsV0::CLICKHOUSE => {
                let connection_string: String = m.remove_required("connection_string")?;
                let table_name: String = m.remove_required("table")?;

                // You can optionally provide a database name in clickhouse.
                // This is similar to schema in other databases such as PG.
                let database_name: Option<String> = m.remove_optional("database")?;

                // Validate
                let access =
                    ClickhouseAccess::new_from_connection_string(connection_string.clone());

                let table_ref =
                    ClickhouseTableRef::new(database_name.as_ref(), table_name.as_str());

                access.validate_table_access(table_ref.as_ref()).await?;

                TableOptionsClickhouse {
                    connection_string,
                    table: table_name,
                    database: database_name,
                }
                .into()
            }
            TableOptionsV0::CASSANDRA => {
                let host: String = m.remove_required("host")?;
                let keyspace: String = m.remove_required("keyspace")?;
                let table: String = m.remove_required("table")?;
                let username: Option<String> = m.remove_optional("username")?;
                let password: Option<String> = m.remove_optional("password")?;
                let access =
                    CassandraAccessState::try_new(host.clone(), username.clone(), password.clone())
                        .await?;
                access.validate_table_access(&keyspace, &table).await?;

                TableOptionsCassandra {
                    host,
                    keyspace,
                    table,
                    username,
                    password,
                }
                .into()
            }
            TableOptionsV0::SQLITE => {
                let location: String = m.remove_required("location")?;
                let table: String = m.remove_required("table")?;
                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }

                TableOptionsV0::Sqlite(TableOptionsObjectStore {
                    location,
                    storage_options,
                    name: table.into(),
                    file_type: None,
                    compression: None,
                    schema_sample_size: None,
                    jaq_filter: None,
                })
            }
            TableOptionsV0::LOCAL => {
                let location: String = m.remove_required("location")?;

                let access = Arc::new(LocalStoreAccess);
                let (file_type, compression) =
                    validate_and_get_file_type_and_compression(access, &location, m).await?;

                let jaq_filter: Option<String> = m.remove_optional("jaq_filter")?;

                TableOptionsLocal {
                    location,
                    file_type: file_type.to_string().to_lowercase(),
                    compression: compression.map(|c| c.to_string()),
                    jaq_filter,
                }
                .into()
            }
            TableOptionsV0::GCS => {
                let service_account_key = creds_options.as_ref().map(|c| match c {
                    CredentialsOptions::Gcp(c) => c.service_account_key.clone(),
                    other => unreachable!("invalid credentials {other} for google cloud storage"),
                });

                let service_account_key =
                    m.remove_optional_or("service_account_key", service_account_key)?;

                let (bucket, location) =
                    get_obj_store_bucket_and_location(m, DatasourceUrlType::Gcs, "bucket")?;

                let access = Arc::new(GcsStoreAccess {
                    bucket: bucket.clone(),
                    service_account_key: service_account_key.clone(),
                    opts: HashMap::new(),
                });
                let (file_type, compression) =
                    validate_and_get_file_type_and_compression(access, &location, m).await?;

                let jaq_filter: Option<String> = m.remove_optional("jaq_filter")?;
                TableOptionsGcs {
                    bucket,
                    service_account_key,
                    location,
                    file_type,
                    compression: compression.map(|c| c.to_string()),
                    jaq_filter,
                }
                .into()
            }
            TableOptionsV0::S3_STORAGE => {
                let creds = creds_options.as_ref().map(|c| match c {
                    CredentialsOptions::Aws(c) => c,
                    other => unreachable!("invalid credentials {other} for aws s3"),
                });

                let (access_key_id, secret_access_key) = match creds {
                    Some(c) => (
                        Some(c.access_key_id.clone()),
                        Some(c.secret_access_key.clone()),
                    ),
                    None => (None, None),
                };

                let access_key_id = m.remove_optional_or("access_key_id", access_key_id)?;
                let secret_access_key =
                    m.remove_optional_or("secret_access_key", secret_access_key)?;

                let region: String = m.remove_required("region")?;

                let (bucket, location) =
                    get_obj_store_bucket_and_location(m, DatasourceUrlType::S3, "bucket")?;

                let access = Arc::new(S3StoreAccess {
                    bucket: bucket.clone(),
                    region: Some(region.clone()),
                    access_key_id: access_key_id.clone(),
                    secret_access_key: secret_access_key.clone(),
                    opts: HashMap::new(),
                });
                let (file_type, compression) =
                    validate_and_get_file_type_and_compression(access, &location, m).await?;

                let jaq_filter: Option<String> = m.remove_optional("jaq_filter")?;

                TableOptionsS3 {
                    region,
                    bucket,
                    access_key_id,
                    secret_access_key,
                    location,
                    file_type: file_type.to_string(),
                    compression: compression.map(|c| c.to_string()),
                    jaq_filter,
                }
                .into()
            }
            TableOptionsV0::AZURE => {
                let (account, access_key) = match creds_options {
                    Some(CredentialsOptions::Azure(c)) => {
                        (Some(c.account_name.clone()), Some(c.access_key.clone()))
                    }
                    Some(other) => {
                        return Err(PlanError::String(format!(
                            "invalid credentials {other} for azure"
                        )))
                    }
                    None => (None, None),
                };

                let account_name = m.remove_required_or("account_name", account)?;
                let access_key = m.remove_required_or("access_key", access_key)?;

                let (container, location) =
                    get_obj_store_bucket_and_location(m, DatasourceUrlType::Azure, "container")?;

                let access = Arc::new(AzureStoreAccess {
                    container,
                    account_name: Some(account_name.clone()),
                    access_key: Some(access_key.clone()),
                    opts: HashMap::new(),
                });

                // TODO: Creating a data source url here is a workaround for
                // getting the path to the file. Since we're using the generic
                // object store access, it requires that "location" is the
                // full url of the object, but that goes against our other
                // assumptions that "location" is just the path.
                let (file_type, compression) =
                    validate_and_get_file_type_and_compression(access.clone(), &location, m)
                        .await?;

                let source_url = format!("{}{}", access.base_url()?, access.path(&location)?);
                let mut opts = StorageOptions::default();
                opts.inner.insert(
                    AzureConfigKey::AccountName.as_ref().to_string(),
                    account_name,
                );
                opts.inner
                    .insert(AzureConfigKey::AccessKey.as_ref().to_string(), access_key);

                let jaq_filter: Option<String> = m.remove_optional("jaq_filter")?;
                TableOptionsV0::Azure(TableOptionsObjectStore {
                    name: None,
                    location: source_url,
                    storage_options: opts,
                    file_type: Some(file_type.to_string()),
                    compression: compression.map(|c| c.to_string()),
                    schema_sample_size: None,
                    jaq_filter,
                })
            }
            TableOptionsV0::DELTA | TableOptionsV0::ICEBERG => {
                let location: String = m.remove_required("location")?;

                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }

                if datasource == TableOptionsV0::DELTA {
                    let _table = load_table_direct(&location, storage_options.clone()).await?;

                    TableOptionsV0::Delta(TableOptionsObjectStore {
                        location,
                        storage_options,
                        name: None,
                        file_type: None,
                        compression: None,
                        schema_sample_size: None,
                        jaq_filter: None,
                    })
                } else {
                    let url = DatasourceUrl::try_new(&location)?;
                    let store = storage_options_into_object_store(&url, &storage_options)?;
                    let _table = IcebergTable::open(url, store).await?;

                    TableOptionsV0::Iceberg(TableOptionsObjectStore {
                        location,
                        storage_options,
                        name: None,
                        file_type: None,
                        compression: None,
                        schema_sample_size: None,
                        jaq_filter: None,
                    })
                }
            }
            TableOptionsV0::LANCE => {
                let location: String = m.remove_required("location")?;
                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }
                // Validate that the table exists.
                let _table = LanceTable::new(&location, storage_options.clone()).await?;
                TableOptionsV0::Lance(TableOptionsObjectStore {
                    location,
                    storage_options,
                    name: None,
                    file_type: None,
                    compression: None,
                    schema_sample_size: None,
                    jaq_filter: None,
                })
            }
            TableOptionsV0::BSON => {
                let location: String = m.remove_required("location")?;
                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }
                let schema_sample_size = Some(
                    storage_options
                        .inner
                        .get("schema_sample_size")
                        .map(|strint| strint.parse())
                        .unwrap_or(Ok(100))?,
                );
                TableOptionsV0::Bson(TableOptionsObjectStore {
                    location,
                    storage_options,
                    name: None,
                    file_type: None,
                    compression: None,
                    schema_sample_size,
                    jaq_filter: None,
                })
            }
            TableOptionsV0::JSON => {
                let location: String = m.remove_required("location")?;
                let jaq_filter: Option<String> = m.remove_optional("jaq_filter")?;

                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }
                let schema_sample_size = Some(
                    storage_options
                        .inner
                        .get("schema_sample_size")
                        .map(|strint| strint.parse())
                        .unwrap_or(Ok(100))?,
                );

                TableOptionsV0::Json(TableOptionsObjectStore {
                    location,
                    storage_options,
                    name: None,
                    file_type: None,
                    compression: None,
                    schema_sample_size,
                    jaq_filter,
                })
            }

            TableOptionsV0::EXCEL => {
                let location: String = m.remove_required("location")?;
                let mut storage_options = StorageOptions::try_from(m)?;
                if let Some(creds) = creds_options {
                    storage_options_with_credentials(&mut storage_options, creds);
                }
                let sheet_name = storage_options
                    .inner
                    .get("sheet_name")
                    .map(|val| val.to_owned());

                let has_header = storage_options
                    .inner
                    .get("has_header")
                    .map(|val| val.parse::<bool>().unwrap_or(true));

                if let DatasourceUrl::File(p) = DatasourceUrl::try_new(&location)? {
                    if !p.exists() {
                        return Err(PlanError::String(
                            "invalid local file path specified".to_string(),
                        ));
                    }
                };

                TableOptionsExcel {
                    location,
                    storage_options,
                    file_type: None,
                    compression: None,
                    sheet_name,
                    has_header,
                }
                .into()
            }
            other => return Err(internal!("unsupported datasource: {}", other)),
        })
    }

    async fn plan_create_external_table(
        &self,
        mut stmt: CreateExternalTableStmt,
    ) -> Result<LogicalPlan> {
        let datasource = normalize_ident(stmt.datasource);

        let tunnel = stmt.tunnel.map(normalize_ident);
        let tunnel_options = self.get_tunnel_opts(&tunnel)?;
        if let Some(tunnel_options) = &tunnel_options {
            // Validate if the tunnel type is supported by the datasource
            validate_table_tunnel_support(&datasource, tunnel_options.as_str()).map_err(|e| {
                PlanError::InvalidExternalTable {
                    source: Box::new(e),
                }
            })?;
        }

        let creds = stmt.credentials.map(normalize_ident);
        let creds_options = self.get_credentials_opts(&creds)?;
        if let Some(creds_options) = &creds_options {
            validate_table_creds_support(&datasource, creds_options.as_str()).map_err(|e| {
                PlanError::InvalidExternalTable {
                    source: Box::new(e),
                }
            })?;
        }

        let schema = stmt
            .columns
            .map(|columns| {
                let fields = columns
                    .into_iter()
                    .map(|coll| -> Result<Field, PlanError> {
                        // check if there is a NOT NULL constraint
                        let has_not_null_constraint = coll
                            .options
                            .into_iter()
                            .any(|k| matches!(k.option, ColumnOption::NotNull));

                        if has_not_null_constraint {
                            Err(PlanError::String(
                                "'NOT NULL' constraint is not supported".to_string(),
                            ))
                        } else {
                            Ok(Field::new(
                                coll.name.to_string(),
                                convert_data_type(&coll.data_type)?,
                                true,
                            ))
                        }
                    })
                    .collect::<Result<Vec<_>, PlanError>>()?;
                Ok::<_, PlanError>(Schema::new(fields))
            })
            .transpose()?;
        let m = &mut stmt.options;

        // The mutator uses the new table options, but the catalog uses the old ones.
        // so we need to convert the old options to the new ones.
        let external_table_options = self
            .get_tbl_opts_from_v0(datasource.as_str(), m, creds_options, tunnel_options)
            .await?;

        let table_name = object_name_to_table_ref(stmt.name)?;

        let plan = CreateExternalTable {
            tbl_reference: self.ctx.resolve_table_ref(table_name)?,
            or_replace: stmt.or_replace,
            if_not_exists: stmt.if_not_exists,
            table_options: external_table_options,
            tunnel,
            schema,
        };

        Ok(plan.into_logical_plan())
    }

    fn plan_create_tunnel(&self, mut stmt: CreateTunnelStmt) -> Result<LogicalPlan> {
        let m = &mut stmt.options;

        let tunnel_type = normalize_ident(stmt.tunnel);

        let options = match tunnel_type.as_str() {
            TunnelOptions::INTERNAL => TunnelOptions::Internal(TunnelOptionsInternal {}),
            TunnelOptions::DEBUG => TunnelOptions::Debug(TunnelOptionsDebug {}),
            TunnelOptions::SSH => {
                let connection_string = get_ssh_conn_str(m)?;
                let ssh_key = SshKey::generate_random()?;

                TunnelOptions::Ssh(TunnelOptionsSsh {
                    connection_string,
                    ssh_key: ssh_key.to_bytes()?,
                })
            }
            other => return Err(internal!("unsupported tunnel: {other}")),
        };

        let name = normalize_ident(stmt.name);

        let plan = CreateTunnel {
            name,
            options,
            if_not_exists: stmt.if_not_exists,
        };

        Ok(plan.into_logical_plan())
    }

    fn plan_create_credentials(&self, mut stmt: PlanCredentialArgs) -> Result<LogicalPlan> {
        let m = &mut stmt.options;

        let provider = normalize_ident(stmt.provider);

        let options = match provider.as_str() {
            CredentialsOptions::DEBUG => {
                let table_type: DebugTableType = m.remove_required("table_type")?;
                CredentialsOptions::Debug(CredentialsOptionsDebug {
                    table_type: table_type.to_string(),
                })
            }
            CredentialsOptions::GCP => {
                let service_account_key = m.remove_required("service_account_key")?;
                CredentialsOptions::Gcp(CredentialsOptionsGcp {
                    service_account_key,
                })
            }
            CredentialsOptions::AWS => {
                let access_key_id = m.remove_required("access_key_id")?;
                let secret_access_key = m.remove_required("secret_access_key")?;
                CredentialsOptions::Aws(CredentialsOptionsAws {
                    access_key_id,
                    secret_access_key,
                })
            }
            CredentialsOptions::AZURE => {
                let account_name = m.remove_required("account_name")?;
                let access_key = m.remove_required("access_key")?;
                CredentialsOptions::Azure(CredentialsOptionsAzure {
                    account_name,
                    access_key,
                })
            }
            CredentialsOptions::OPENAI => {
                let api_key = m.remove_required("api_key")?;
                let api_base = m.remove_optional("api_base")?;
                let org_id = m.remove_optional("org_id")?;

                CredentialsOptions::OpenAI(CredentialsOptionsOpenAI {
                    api_key,
                    api_base,
                    org_id,
                })
            }
            other => return Err(internal!("unsupported credentials provider: {other}")),
        };

        let name = normalize_ident(stmt.name);

        Ok(CreateCredentials {
            name,
            options,
            comment: stmt.comment,
            or_replace: stmt.or_replace,
        }
        .into_logical_plan())
    }

    async fn plan_statement(&self, statement: ast::Statement) -> Result<LogicalPlan> {
        let state = self.ctx.df_ctx().state();
        let mut context_provider = PartialContextProvider::new(self.ctx, &state)?;
        match statement {
            ast::Statement::StartTransaction { .. } => Ok(TransactionPlan::Begin.into()),
            ast::Statement::Commit { .. } => Ok(TransactionPlan::Commit.into()),
            ast::Statement::Rollback { .. } => Ok(TransactionPlan::Abort.into()),

            ast::Statement::Query(q) => {
                let mut planner = SqlQueryPlanner::new(&mut context_provider);
                let plan = planner.query_to_plan(*q).await?;
                Ok(LogicalPlan::Datafusion(plan))
            }

            ast::Statement::Explain {
                describe_alias: DescribeAlias::Explain,
                verbose,
                statement,
                analyze,
                ..
            } => {
                let mut planner = SqlQueryPlanner::new(&mut context_provider);
                let plan = planner
                    .explain_statement_to_plan(verbose, analyze, *statement)
                    .await?;
                Ok(LogicalPlan::Datafusion(plan))
            }
            // DESCRIBE <table_name>
            ast::Statement::ExplainTable {
                describe_alias: DescribeAlias::Describe,
                table_name,
                ..
            } => {
                validate_object_name(&table_name)?;
                let table_name = object_name_to_table_ref(table_name)?;
                let resolver = EntryResolver::from_context(self.ctx);
                let entry = resolver
                    .resolve_entry_from_reference(table_name.clone())?
                    .try_into_table_entry()?;

                let plan = DescribeTable { entry };

                Ok(plan.into_logical_plan())
            }

            ast::Statement::CreateSchema {
                schema_name,
                if_not_exists,
            } => {
                // TODO: Schema Authorization
                let schema_name = match schema_name {
                    ast::SchemaName::Simple(name) => {
                        validate_object_name(&name)?;
                        object_name_to_schema_ref(name)?
                    }
                    ast::SchemaName::UnnamedAuthorization(ident) => {
                        validate_ident(&ident)?;
                        OwnedSchemaReference::Bare {
                            schema: normalize_ident(ident).into(),
                        }
                    }
                    ast::SchemaName::NamedAuthorization(name, ident) => {
                        validate_object_name(&name)?;
                        validate_ident(&ident)?;
                        object_name_to_schema_ref(name)?
                    }
                };

                Ok(CreateSchema {
                    schema_reference: self.ctx.resolve_schema_ref(schema_name),
                    if_not_exists,
                }
                .into_logical_plan())
            }

            // Normal tables OR Tables generated from a source query.
            // CREATE TABLE
            // CREATE TABLE table2 AS (SELECT * FROM table1);
            ast::Statement::CreateTable {
                external: false,
                if_not_exists,
                or_replace,
                engine: None,
                name,
                columns,
                query,
                temporary,
                ..
            } => {
                validate_object_name(&name)?;
                let table_name = object_name_to_table_ref(name)?;

                let (source, arrow_cols) = if let Some(q) = query {
                    let mut ctx = context_provider;

                    let mut planner = SqlQueryPlanner::new(&mut ctx);

                    let source = planner.query_to_plan(*q).await?;
                    let df_fields = source.schema().fields();

                    let mut columns = columns.into_iter();
                    let mut fields = Vec::with_capacity(df_fields.len());
                    for df_field in df_fields {
                        let field = df_field.field().as_ref().clone();
                        let field = if let Some(column) = columns.next() {
                            // If we have a cast for the column, we can update the schema.
                            validate_ident(&column.name)?;
                            let name = normalize_ident(column.name);
                            let data_type = convert_data_type(&column.data_type)?;
                            field.with_name(name).with_data_type(data_type)
                        } else {
                            field
                        };
                        fields.push(field);
                    }

                    // Update the source plan with the new schema casts and alias.
                    let project_exprs: Vec<_> = fields
                        .iter()
                        .zip(df_fields.iter())
                        .map(|(field, df_field)| {
                            cast(
                                col(df_field.unqualified_column()),
                                field.data_type().clone(),
                            )
                            .alias(field.name())
                        })
                        .collect();

                    let source = LogicalPlanBuilder::from(source)
                        .project(project_exprs)?
                        .build()?;

                    (Some(source), fields)
                } else {
                    let mut arrow_cols = Vec::with_capacity(columns.len());
                    for column in columns.into_iter() {
                        validate_ident(&column.name)?;
                        let name = normalize_ident(column.name);
                        let data_type = convert_data_type(&column.data_type)?;
                        let field = Field::new(name, data_type, /* nullable = */ true);
                        arrow_cols.push(field);
                    }
                    (None, arrow_cols)
                };

                if temporary {
                    let table_name = match table_name {
                        TableReference::Bare { table } => table.into_owned(),
                        _ => return Err(internal!("cannot specify schema with temporary tables")),
                    };
                    let df_schema = Schema::new(arrow_cols.clone());
                    let df_schema = df_schema.to_dfschema_ref()?;

                    let plan = CreateTempTable {
                        tbl_reference: FullObjectReference {
                            database: DEFAULT_CATALOG.into(),
                            schema: CURRENT_SESSION_SCHEMA.into(),
                            name: table_name.into(),
                        },
                        schema: df_schema,
                        if_not_exists,
                        or_replace,
                        source,
                    };

                    Ok(plan.into_logical_plan())
                } else {
                    let df_schema = Schema::new(arrow_cols.clone());
                    let df_schema = df_schema.to_dfschema_ref()?;
                    let create_table = CreateTable {
                        tbl_reference: self.ctx.resolve_table_ref(table_name)?,
                        schema: df_schema,
                        if_not_exists,
                        or_replace,
                        source,
                    };
                    Ok(create_table.into_logical_plan())
                }
            }

            // Views
            ast::Statement::CreateView {
                or_replace,
                materialized: false,
                name,
                columns,
                query,
                options,
                ..
            } => {
                validate_object_name(&name)?;
                let name = object_name_to_table_ref(name)?;

                if !matches!(options, ast::CreateTableOptions::None) {
                    return Err(PlanError::UnsupportedFeature("view options"));
                }

                if !matches!(
                    query.body.as_ref(),
                    ast::SetExpr::Values(_) | ast::SetExpr::Query(_) | ast::SetExpr::Select(_)
                ) {
                    return Err(PlanError::InvalidViewStatement {
                        msg: "view body must either be a SELECT or VALUES statement",
                    });
                }

                let query_string = query.to_string();

                // Check that this is a valid body.
                // TODO: Avoid cloning.
                let mut planner = SqlQueryPlanner::new(&mut context_provider);
                let input = planner.query_to_plan(*query).await?;

                let columns: Vec<_> = columns
                    .into_iter()
                    .map(|col| normalize_ident(col.name))
                    .collect();
                // Only validate number of aliases equals number of fields in
                // the ouput if aliases were actually provided.
                if !columns.is_empty() && input.schema().fields().len() != columns.len() {
                    Err(PlanError::InvalidNumberOfAliasesForView {
                        sql: query_string,
                        aliases: columns,
                    })
                } else {
                    Ok(CreateView {
                        view_reference: self.ctx.resolve_table_ref(name)?,
                        sql: query_string,
                        columns,
                        or_replace,
                    }
                    .into_logical_plan())
                }
            }

            ast::Statement::Insert {
                or: None,
                into: _,
                table_name,
                columns,
                overwrite: false,
                source,
                partitioned: None,
                after_columns,
                table: false,
                on: None,
                returning: None,
                ignore: _,
                ..
            } if after_columns.is_empty() => {
                let source = source.ok_or(PlanError::InvalidInsertStatement {
                    msg: "Nothing to insert: source empty",
                })?;

                validate_object_name(&table_name)?;
                let table_name = object_name_to_table_ref(table_name)?;

                let columns = columns
                    .into_iter()
                    .map(|col| {
                        validate_ident(&col)?;
                        Ok(normalize_ident(col))
                    })
                    .collect::<Result<Vec<_>>>()?;

                let mut planner = SqlQueryPlanner::new(&mut context_provider);
                let source = planner
                    .insert_to_source_plan(&table_name, &columns, source)
                    .await?;

                let access_mode = self
                    .get_access_mode(table_name.clone())?
                    .unwrap_or(SourceAccessMode::ReadOnly);

                if !access_mode.has_write_access() {
                    return Err(PlanError::ObjectNotAllowedToWriteInto(
                        table_name.to_owned_reference(),
                    ));
                }

                let state = self.ctx.df_ctx().state();
                let mut ctx_provider = PartialContextProvider::new(self.ctx, &state)?;

                let provider = ctx_provider.table_provider(table_name).await?;

                let (runtime_preference, provider) = match (
                    provider.preference,
                    provider
                        .provider
                        .as_any()
                        .downcast_ref::<StubRemoteTableProvider>(),
                ) {
                    (RuntimePreference::Remote, Some(stub)) => (
                        RuntimePreference::Remote,
                        ProviderReference::RemoteReference(stub.id()),
                    ),
                    _ => (
                        RuntimePreference::Local,
                        ProviderReference::Provider(provider.provider),
                    ),
                };

                Ok(Insert {
                    source,
                    provider,
                    runtime_preference,
                }
                .into_logical_plan())
            }

            ast::Statement::AlterTable {
                name,
                mut operations,
                ..
            } => {
                if operations.len() != 1 {
                    return Err(PlanError::UnsupportedFeature(
                        "ALTER TABLE with multiple operations",
                    ));
                }
                let operation = operations.pop().unwrap();

                match operation {
                    ast::AlterTableOperation::RenameTable { table_name } => {
                        validate_object_name(&name)?;
                        let name = object_name_to_table_ref(name)?;
                        let name = self.ctx.resolve_table_ref(name)?;

                        let schema = name.schema.into_owned();
                        let name = name.name.into_owned();

                        let new_name = match table_name {
                            ObjectName(mut objs) if objs.len() == 1 => objs.pop().unwrap(),
                            _ => {
                                return Err(PlanError::InvalidAlterStatement {
                                    msg: "new table name should be a valid table identifier",
                                })
                            }
                        };
                        validate_ident(&new_name)?;
                        let new_name = normalize_ident(new_name);

                        Ok(AlterTable {
                            schema,
                            name,
                            operation: AlterTableOperation::RenameTable { new_name },
                        }
                        .into_logical_plan())
                    }
                    other => Err(PlanError::UnsupportedSQLStatement(other.to_string())),
                }
            }

            // Drop tables
            ast::Statement::Drop {
                object_type: ObjectType::Table,
                if_exists,
                names,
                ..
            } => {
                let mut refs = Vec::with_capacity(names.len());
                for name in names.into_iter() {
                    validate_object_name(&name)?;
                    let r = object_name_to_table_ref(name)?;
                    refs.push(self.ctx.resolve_table_ref(r)?);
                }

                let plan = DropTables {
                    if_exists,
                    tbl_references: refs,
                };
                Ok(plan.into_logical_plan())
            }

            // Drop views
            ast::Statement::Drop {
                object_type: ObjectType::View,
                if_exists,
                names,
                ..
            } => {
                let mut refs = Vec::with_capacity(names.len());
                for name in names.into_iter() {
                    validate_object_name(&name)?;
                    let r = object_name_to_table_ref(name)?;
                    refs.push(self.ctx.resolve_table_ref(r)?);
                }
                Ok(DropViews {
                    if_exists,
                    view_references: refs,
                }
                .into_logical_plan())
            }

            // Drop schemas
            ast::Statement::Drop {
                object_type: ObjectType::Schema,
                if_exists,
                cascade,
                names,
                ..
            } => {
                let mut refs = Vec::with_capacity(names.len());
                for name in names.into_iter() {
                    validate_object_name(&name)?;
                    let r = object_name_to_schema_ref(name)?;
                    refs.push(self.ctx.resolve_schema_ref(r));
                }
                Ok(DropSchemas {
                    if_exists,
                    schema_references: refs,
                    cascade,
                }
                .into_logical_plan())
            }

            // "SET ...".
            //
            // NOTE: Only session local variables are supported. Transaction
            // local variables behave the same as session local (they're not
            // reset on transaction abort/commit).
            ast::Statement::SetVariable {
                hivevar: false,
                variable,
                value,
                ..
            } => {
                let plan = SetVariable::try_new(variable.to_string(), value)?;
                Ok(plan.into_logical_plan())
            }
            // "SHOW ..."
            //
            // Show the value of a variable.
            ast::Statement::ShowVariable { variable } => {
                // Normalize variables
                let mut variable: Vec<_> = variable.into_iter().map(normalize_ident).collect();

                let variable = if is_show_transaction_isolation_level(&variable) {
                    // SHOW TRANSACTION ISOLATION LEVEL
                    // Alias of "SHOW transaction_isolation".
                    "transaction_isolation".to_string()
                } else if variable.len() != 1 {
                    return Err(internal!(
                        "expecting only one variable to show, found: {variable:?}"
                    ));
                } else {
                    variable.pop().unwrap()
                };

                Ok(ShowVariable::new(variable).into_logical_plan())
            }

            // "DELETE FROM <table> WHERE <expression>"
            //
            // deletes rows from a table that matches the expression.
            // or all the rows if no expression is provided.
            ast::Statement::Delete {
                tables,
                from: FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from),
                using: None,
                selection,
                returning: None,
                // TODO: Order by and limit
                order_by: _,
                limit: _,
            } if tables.is_empty() => {
                let (table_name, schema) = match from.len() {
                    0 => {
                        return Err(PlanError::InvalidDeleteStatement {
                            msg: "DELETE FROM should have atleast one table name",
                        })
                    }
                    1 => {
                        let table_factor = from[0].relation.clone();
                        let table_name = match table_factor {
                            ast::TableFactor::Table { name, .. } => name,
                            _ => {
                                return Err(PlanError::UnsupportedFeature(
                                    "DELETE from TableWithJoins",
                                ))
                            }
                        };
                        validate_object_name(&table_name)?;
                        let table_name = object_name_to_table_ref(table_name)?;

                        let table_source = context_provider
                            .get_table_source(table_name.clone())
                            .await?;
                        let schema = table_source.schema().to_dfschema()?;
                        (table_name, schema)
                    }
                    _ => return Err(PlanError::UnsupportedFeature("DELETE from multiple tables")),
                };

                let where_expr = if let Some(where_expr) = selection {
                    let mut planner = SqlQueryPlanner::new(&mut context_provider);
                    Some(
                        planner
                            .sql_to_expr(where_expr, &schema, &mut PlannerContext::new())
                            .await?,
                    )
                } else {
                    None
                };

                let resolver = EntryResolver::from_context(self.ctx);
                let ent = resolver
                    .resolve_entry_from_reference(table_name)?
                    .try_into_table_entry()?;
                // External deletes not supported yet.
                if ent.meta.external {
                    return Err(PlanError::UnsupportedFeature("DELETE with external tables"));
                }

                Ok(Delete {
                    table: ent,
                    where_expr,
                }
                .into_logical_plan())
            }

            // "UPDATE <table_name> SET <col1> = <value_expression> WHERE <expression>"
            //
            // update column values of a table for rows that match the expression.
            // or all the rows if no expression is provided.
            ast::Statement::Update {
                table,
                assignments,
                from: None,
                selection,
                returning: None,
            } => {
                let table_factor = table.relation.clone();
                let table_name = match table_factor {
                    ast::TableFactor::Table { name, .. } => name,
                    _ => return Err(PlanError::UnsupportedFeature("UPDATE from TableWithJoins")),
                };
                validate_object_name(&table_name)?;
                let table_name = object_name_to_table_ref(table_name)?;

                let table_source = context_provider
                    .get_table_source(table_name.clone())
                    .await?;
                let schema = table_source.schema().to_dfschema()?;

                let mut planner = SqlQueryPlanner::new(&mut context_provider);
                let mut updates = Vec::new();

                for assignment in assignments {
                    if assignment.id.len() == 1 {
                        let column = assignment.id.last().unwrap().value.clone();
                        let update_value = planner
                            .sql_to_expr(assignment.value, &schema, &mut PlannerContext::new())
                            .await?;
                        updates.push((column, update_value));
                    } else {
                        return Err(PlanError::UnsupportedSQLStatement(
                            "Update statement with table reference in column name".to_string(),
                        ));
                    }
                }

                let where_expr = if let Some(where_expr) = selection {
                    Some(
                        planner
                            .sql_to_expr(where_expr, &schema, &mut PlannerContext::new())
                            .await?,
                    )
                } else {
                    None
                };

                let resolver = EntryResolver::from_context(self.ctx);
                let ent = resolver
                    .resolve_entry_from_reference(table_name)?
                    .try_into_table_entry()?;
                // External updates not supported yet.
                if ent.meta.external {
                    return Err(PlanError::UnsupportedFeature("UPDATE with external tables"));
                }

                Ok(Update {
                    table: ent,
                    updates,
                    where_expr,
                }
                .into_logical_plan())
            }

            stmt => Err(PlanError::UnsupportedSQLStatement(stmt.to_string())),
        }
    }

    fn plan_drop_database(&self, stmt: DropDatabaseStmt) -> Result<LogicalPlan> {
        let mut names = Vec::with_capacity(stmt.names.len());
        for name in stmt.names.into_iter() {
            validate_ident(&name)?;
            let name = normalize_ident(name);
            names.push(name);
        }

        Ok(DropDatabase {
            names,
            if_exists: stmt.if_exists,
        }
        .into_logical_plan())
    }

    fn plan_drop_tunnel(&self, stmt: DropTunnelStmt) -> Result<LogicalPlan> {
        let mut names = Vec::with_capacity(stmt.names.len());
        for name in stmt.names.into_iter() {
            validate_ident(&name)?;
            let name = normalize_ident(name);
            names.push(name);
        }

        Ok(DropTunnel {
            names,
            if_exists: stmt.if_exists,
        }
        .into_logical_plan())
    }

    fn plan_drop_credentials(&self, stmt: DropCredentialsStmt) -> Result<LogicalPlan> {
        let mut names = Vec::with_capacity(stmt.names.len());
        for name in stmt.names.into_iter() {
            validate_ident(&name)?;
            let name = normalize_ident(name);
            names.push(name);
        }

        Ok(DropCredentials {
            names,
            if_exists: stmt.if_exists,
        }
        .into_logical_plan())
    }

    fn plan_alter_tunnel(&self, stmt: AlterTunnelStmt) -> Result<LogicalPlan> {
        validate_ident(&stmt.name)?;
        let name = normalize_ident(stmt.name);

        let plan = match stmt.action {
            AlterTunnelAction::RotateKeys => {
                let new_ssh_key = SshKey::generate_random()?;
                let new_ssh_key = new_ssh_key.to_bytes()?;
                AlterTunnelRotateKeys {
                    name,
                    if_exists: stmt.if_exists,
                    new_ssh_key,
                }
            }
        };

        Ok(plan.into_logical_plan())
    }

    fn plan_alter_database(&self, stmt: AlterDatabaseStmt) -> Result<LogicalPlan> {
        validate_ident(&stmt.name)?;
        let name = normalize_ident(stmt.name);

        let operation = match stmt.operation {
            parser::AlterDatabaseOperation::RenameDatabase { new_name } => {
                validate_ident(&new_name)?;
                let new_name = normalize_ident(new_name);
                AlterDatabaseOperation::RenameDatabase { new_name }
            }
            parser::AlterDatabaseOperation::SetAccessMode { access_mode } => {
                let access_mode = SourceAccessMode::from_str(&access_mode.value)
                    .map_err(|e| PlanError::String(format!("{e}")))?;
                AlterDatabaseOperation::SetAccessMode { access_mode }
            }
        };

        Ok(AlterDatabase { name, operation }.into_logical_plan())
    }

    fn plan_alter_table_extension(&self, stmt: AlterTableStmtExtension) -> Result<LogicalPlan> {
        validate_object_name(&stmt.name)?;
        let name = object_name_to_table_ref(stmt.name)?;
        let name = self.ctx.resolve_table_ref(name)?;
        let schema = name.schema.into_owned();
        let name = name.name.into_owned();

        let operation = match stmt.operation {
            parser::AlterTableOperationExtension::SetAccessMode { access_mode } => {
                let access_mode = SourceAccessMode::from_str(&access_mode.value)
                    .map_err(|e| PlanError::String(format!("{e}")))?;
                AlterTableOperation::SetAccessMode { access_mode }
            }
        };

        Ok(AlterTable {
            schema,
            name,
            operation,
        }
        .into_logical_plan())
    }

    async fn plan_copy_to(&self, stmt: CopyToStmt) -> Result<LogicalPlan> {
        let query = match stmt.source {
            CopyToSource::Table(table) => {
                validate_object_name(&table)?;
                let table_ref = object_name_to_table_ref(table)?;
                let table_ref = quoted_table_ref(table_ref);
                let query = format!("SELECT * FROM {table_ref}");
                match parser::parse_sql(&query)?.pop_front() {
                    Some(StatementWithExtensions::Statement(ast::Statement::Query(q))) => *q,
                    _ => unreachable!(),
                }
            }
            CopyToSource::Query(query) => query,
        };

        let state = self.ctx.df_ctx().state();
        let mut context_provider = PartialContextProvider::new(self.ctx, &state)?;
        let mut planner = SqlQueryPlanner::new(&mut context_provider);
        let source = planner.query_to_plan(query).await?;

        let mut m = stmt.options;

        let destination = normalize_ident(stmt.dest);

        // We currently support two versions of COPY TO:
        //
        // 1: COPY <source> TO <s3|gcs|azure> OPTIONS (...)
        // 2: COPY <source> TO <dest> OPTIONS (...)
        //
        // Where the first matches on fixed keywords, and the second matches on
        // the full object path (e.g. 'gs://bucket/object.csv'). This statement
        // is what lets us differentiate between those, and if `url` is `None`,
        // we'll resolve the actual object destination from the OPTIONS down
        // below.
        let (destination, uri, location) = if matches!(
            destination.as_str(),
            CopyToDestinationOptions::LOCAL
                | CopyToDestinationOptions::GCS
                | CopyToDestinationOptions::S3_STORAGE
                | CopyToDestinationOptions::AZURE
        ) {
            let location: String = m.remove_required("location")?;
            let (uri, location) = match DatasourceUrl::try_new(&location) {
                Ok(uri @ DatasourceUrl::Url(_)) => {
                    let location = uri.path().into_owned();
                    (Some(uri), location)
                }
                _ => (None, location),
            };
            (destination.as_str(), uri, location)
        } else {
            let uri = DatasourceUrl::try_new(&destination)?;
            let destination = match uri.datasource_url_type() {
                DatasourceUrlType::File => CopyToDestinationOptions::LOCAL,
                DatasourceUrlType::Gcs => CopyToDestinationOptions::GCS,
                DatasourceUrlType::S3 => CopyToDestinationOptions::S3_STORAGE,
                DatasourceUrlType::Azure => CopyToDestinationOptions::AZURE,
                DatasourceUrlType::Http => return Err(internal!("invalid URL scheme")),
            };
            let location = uri.path().into_owned();
            (destination, Some(uri), location)
        };

        let creds = stmt.credentials.map(normalize_ident);
        let creds_options = self.get_credentials_opts(&creds)?;
        if let Some(creds_options) = &creds_options {
            validate_copyto_dest_creds_support(destination, creds_options.as_str()).map_err(
                |e| PlanError::InvalidCopyToStatement {
                    source: Box::new(e),
                },
            )?;
        }

        fn get_bucket(
            m: &mut StatementOptions,
            uri: &Option<DatasourceUrl>,
            bucket_key: &str,
        ) -> Result<String> {
            let bucket = match uri.as_ref() {
                Some(u) => u
                    .host()
                    .ok_or(internal!("missing {bucket_key} name in URL"))?
                    .to_string(),
                None => m.remove_required(bucket_key)?,
            };
            Ok(bucket)
        }

        let dest = match destination {
            CopyToDestinationOptions::LOCAL => {
                CopyToDestinationOptions::Local(CopyToDestinationOptionsLocal { location })
            }
            CopyToDestinationOptions::GCS => {
                let service_account_key = creds_options.as_ref().map(|c| match c {
                    CredentialsOptions::Gcp(c) => c.service_account_key.clone(),
                    other => unreachable!("invalid credentials {other} for google cloud storage"),
                });

                let service_account_key =
                    m.remove_optional_or("service_account_key", service_account_key)?;

                let bucket = get_bucket(&mut m, &uri, "bucket")?;

                CopyToDestinationOptions::Gcs(CopyToDestinationOptionsGcs {
                    service_account_key,
                    bucket,
                    location,
                })
            }
            CopyToDestinationOptions::S3_STORAGE => {
                let creds = creds_options.as_ref().map(|c| match c {
                    CredentialsOptions::Aws(c) => c,
                    other => unreachable!("invalid credentials {other} for aws s3"),
                });

                let (access_key_id, secret_access_key) = match creds {
                    Some(c) => (
                        Some(c.access_key_id.clone()),
                        Some(c.secret_access_key.clone()),
                    ),
                    None => (None, None),
                };

                let access_key_id = m.remove_optional_or("access_key_id", access_key_id)?;
                let secret_access_key =
                    m.remove_optional_or("secret_access_key", secret_access_key)?;

                let region = m.remove_required("region")?;
                let bucket = get_bucket(&mut m, &uri, "bucket")?;

                CopyToDestinationOptions::S3(CopyToDestinationOptionsS3 {
                    access_key_id,
                    secret_access_key,
                    region,
                    bucket,
                    location,
                })
            }
            CopyToDestinationOptions::AZURE => {
                let creds = match creds_options.as_ref() {
                    Some(CredentialsOptions::Azure(c)) => Some(c),
                    Some(other) => {
                        return Err(PlanError::String(format!(
                            "invalid credentials {other} for azure"
                        )))
                    }
                    None => None,
                };

                // Get account and access key from credentials if provided. If
                // not provided, require that they've been passed in through
                // OPTIONS.
                let (account, access_key) = match creds {
                    Some(c) => (c.account_name.clone(), c.access_key.clone()),
                    None => (
                        m.remove_required("account_name")?,
                        m.remove_required("access_key")?,
                    ),
                };

                let container = get_bucket(&mut m, &uri, "container")?;

                CopyToDestinationOptions::Azure(CopyToDestinationOptionsAzure {
                    account,
                    access_key,
                    container,
                    location,
                })
            }
            other => {
                return Err(internal!(
                    "unsupported destination for copying data: {other}"
                ))
            }
        };

        let loc = dest.location();
        let loc = Path::new(loc);
        let ext = loc
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_lowercase());

        let format = match stmt
            .format
            .as_ref()
            .map(|f| f.value.as_str())
            // Choose from specified format "OR" from location.
            .or(ext.as_deref())
        {
            None => {
                // TODO: Choose the default based on destination.
                CopyToFormatOptions::default()
            }
            Some(CopyToFormatOptions::CSV) => {
                let delim = m.remove_optional::<char>("delimeter")?.unwrap_or(',');
                let header = m.remove_optional::<bool>("header")?.unwrap_or(true);
                CopyToFormatOptions::Csv(CopyToFormatOptionsCsv {
                    delim: delim as u8,
                    header,
                })
            }
            Some(CopyToFormatOptions::PARQUET) => {
                let row_group_size = m
                    .remove_optional::<usize>("row_group_size")?
                    .unwrap_or(122880);
                CopyToFormatOptions::Parquet(CopyToFormatOptionsParquet { row_group_size })
            }
            Some(CopyToFormatOptions::JSON) => {
                let array = m.remove_optional::<bool>("array")?.unwrap_or(false);
                CopyToFormatOptions::Json(CopyToFormatOptionsJson { array })
            }
            Some(CopyToFormatOptions::BSON) => {
                CopyToFormatOptions::Bson(CopyToFormatOptionsBson {})
            }
            Some(CopyToFormatOptions::LANCE) => {
                CopyToFormatOptions::Lance(CopyToFormatOptionsLance {
                    max_rows_per_file: m.remove_optional("max_rows_per_file")?,
                    max_rows_per_group: m.remove_optional("max_rows_per_group")?,
                    max_bytes_per_file: m.remove_optional("max_bytes_per_file")?,
                    input_batch_size: m.remove_optional("input_batch_size")?,
                })
            }
            Some(CopyToFormatOptions::DELTA) => {
                CopyToFormatOptions::Delta(CopyToFormatOptionsDelta {})
            }
            Some(other) => return Err(internal!("unsupported output format: {other}")),
        };

        validate_copyto_dest_format_support(dest.as_str(), format.as_str()).map_err(|e| {
            PlanError::InvalidExternalTable {
                source: Box::new(e),
            }
        })?;

        Ok(CopyTo {
            format,
            dest,
            source,
        }
        .into_logical_plan())
    }

    fn get_tunnel_opts(&self, tunnel: &Option<String>) -> Result<Option<TunnelOptions>> {
        // Check if the tunnel exists, get tunnel options and pass them on for
        // connection validation.
        let tunnel_options = if let Some(tunnel) = &tunnel {
            let ent = self
                .ctx
                .get_session_catalog()
                .resolve_tunnel(tunnel)
                .ok_or(PlanError::InvalidTunnel {
                    tunnel: tunnel.to_owned(),
                    reason: "does not exist".to_string(),
                })?;
            Some(ent.options.clone())
        } else {
            None
        };
        Ok(tunnel_options)
    }

    fn get_credentials_opts(
        &self,
        credentials: &Option<String>,
    ) -> Result<Option<CredentialsOptions>> {
        // Check if the credentials exists, get credentials options and pass
        // them on for connection validation.
        let credentials_options = if let Some(credentials) = &credentials {
            let ent = self
                .ctx
                .get_session_catalog()
                .resolve_credentials(credentials)
                .ok_or(PlanError::InvalidCredentials {
                    credentials: credentials.to_owned(),
                    reason: "does not exist".to_string(),
                })?;
            Some(ent.options.clone())
        } else {
            None
        };
        Ok(credentials_options)
    }

    fn get_access_mode(&self, table_ref: TableReference<'a>) -> Result<Option<SourceAccessMode>> {
        let resolver = EntryResolver::from_context(self.ctx);
        let ent = resolver.resolve_entry_from_reference(table_ref)?;

        Ok(match ent {
            ResolvedEntry::NeedsExternalResolution {
                db_ent: &DatabaseEntry { access_mode, .. },
                ..
            }
            | ResolvedEntry::Entry(CatalogEntry::Database(DatabaseEntry { access_mode, .. }))
            | ResolvedEntry::Entry(CatalogEntry::Table(TableEntry { access_mode, .. })) => {
                Some(access_mode)
            }
            _ => None,
        })
    }
}

/// Get the object store bucket and location.
fn get_obj_store_bucket_and_location(
    m: &mut StatementOptions,
    ty: DatasourceUrlType,
    bucket_key: &str,
) -> Result<(String, String)> {
    let location: String = m.remove_required("location")?;
    Ok(match DatasourceUrl::try_new(&location) {
        Ok(u @ DatasourceUrl::Url(_)) => {
            if u.datasource_url_type() != ty {
                return Err(PlanError::String(format!(
                    "expected {} URL, found {}",
                    ty,
                    u.datasource_url_type()
                )));
            }
            let bucket = u.host().ok_or_else(|| {
                PlanError::String(format!("missing {} name in {} URL", bucket_key, ty))
            })?;
            let path = u.path();
            (bucket.to_owned(), path.into_owned())
        }
        _ => {
            let bucket = m.remove_required(bucket_key)?;
            (bucket, location)
        }
    })
}

/// Creates an accessor from object store external table and validates if the
/// location returns any objects. If objects are returned, tries to get the file
/// type and compression of the object.
async fn validate_and_get_file_type_and_compression(
    access: Arc<dyn ObjStoreAccess>,
    path: impl AsRef<str>,
    m: &mut StatementOptions,
) -> Result<(String, Option<CompressionTypeVariant>)> {
    let path = path.as_ref();
    let accessor =
        ObjStoreAccessor::new(access.clone()).map_err(|e| PlanError::InvalidExternalTable {
            source: Box::new(e),
        })?;

    let objects =
        accessor
            .list_globbed(path)
            .await
            .map_err(|e| PlanError::InvalidExternalTable {
                source: Box::new(e),
            })?;

    if objects.is_empty() {
        return Err(PlanError::InvalidExternalTable {
            source: Box::new(internal!("object '{path}' not found")),
        });
    }

    let compression =
        match m.remove_optional::<parser::options::CompressionTypeVariant>("compression")? {
            Some(compression) => Some(convert(compression)),
            None => objects
                .first()
                .ok_or_else(|| PlanError::InvalidExternalTable {
                    source: Box::new(internal!("object '{path} not found'")),
                })?
                .location
                .extension()
                .and_then(|ext| ext.parse().ok()),
        };

    let file_type = match m.remove_optional("file_type")? {
        Some(file_type) => file_type,
        None => {
            let mut ft = None;
            for obj in objects {
                ft = match file_type_from_path(&obj.location) {
                    Ok(file_type) => Some(file_type.to_string()),
                    Err(_) => match obj.location.extension() {
                        Some("bson") => Some("bson".to_string()),
                        _ => continue,
                    },
                };
            }

            ft.ok_or_else(|| PlanError::InvalidExternalTable {
                source: Box::new(internal!(
                    "unable to resolve file type from the objects, try passing `file_type` option"
                )),
            })?
            .to_string()
        }
    };

    Ok((file_type, compression))
}

/// Resolves an ident (unquoted -> lowercase else case sensitive).
fn normalize_ident(ident: Ident) -> String {
    let normalizer = IdentNormalizer::new(/* normalize = */ true);
    normalizer.normalize(ident)
}

fn object_name_to_table_ref(name: ObjectName) -> Result<OwnedTableReference> {
    let r = object_name_to_table_reference(convert(name), /* enable_normalization = */ true)?;
    Ok(r)
}

fn quoted_table_ref(table_ref: TableReference<'_>) -> String {
    match table_ref {
        TableReference::Bare { table } => format!("{table:?}"),
        TableReference::Partial { schema, table } => format!("{schema:?}.{table:?}"),
        TableReference::Full {
            catalog,
            schema,
            table,
        } => format!("{catalog:?}.{schema:?}.{table:?}"),
    }
}

fn object_name_to_schema_ref(name: ObjectName) -> Result<OwnedSchemaReference> {
    let r = match object_name_to_table_ref(name)? {
        // Table becomes the schema and schema becomes the catalog.
        OwnedTableReference::Bare { table } => OwnedSchemaReference::Bare { schema: table },
        OwnedTableReference::Partial { schema, table } => OwnedSchemaReference::Full {
            schema: table,
            catalog: schema,
        },
        tr => return Err(internal!("invalid schema object: {tr}")),
    };
    Ok(r)
}

/// Convert a ast data type to an arrow data type.
///
/// NOTE: This and `convert_simple_data_type` were both taken from datafusion's
/// sql planner. These functions were made internal in version 15.0. Light
/// modifications were made to fit our use case.
fn convert_data_type(sql_type: &ast::DataType) -> Result<DataType> {
    match sql_type {
        ast::DataType::Array(ast::ArrayElemTypeDef::AngleBracket(inner_sql_type))
        | ast::DataType::Array(ast::ArrayElemTypeDef::SquareBracket(inner_sql_type)) => {
            let data_type = convert_simple_data_type(inner_sql_type)?;

            Ok(DataType::List(Arc::new(Field::new(
                "field", data_type, true,
            ))))
        }
        ast::DataType::Array(ast::ArrayElemTypeDef::None) => {
            Err(internal!("Arrays with unspecified type is not supported",))
        }
        other => convert_simple_data_type(other),
    }
}

// TODO: We already copy this in by way of the `datafusion_ext` crate. Is there
// a way to ensure we only have a single copy?
fn convert_simple_data_type(sql_type: &ast::DataType) -> Result<DataType> {
    match sql_type {
            ast::DataType::Boolean | ast::DataType::Bool => Ok(DataType::Boolean),
            ast::DataType::TinyInt(_) => Ok(DataType::Int8),
            ast::DataType::SmallInt(_) | ast::DataType::Int2(_) => Ok(DataType::Int16),
            ast::DataType::Int(_) | ast::DataType::Integer(_) | ast::DataType::Int4(_) => Ok(DataType::Int32),
            ast::DataType::BigInt(_) | ast::DataType::Int8(_) => Ok(DataType::Int64),
            ast::DataType::UnsignedTinyInt(_) => Ok(DataType::UInt8),
            ast::DataType::UnsignedSmallInt(_) | ast::DataType::UnsignedInt2(_) => Ok(DataType::UInt16),
            ast::DataType::UnsignedInt(_) | ast::DataType::UnsignedInteger(_) | ast::DataType::UnsignedInt4(_) => {
                Ok(DataType::UInt32)
            }
            ast::DataType::UnsignedBigInt(_) | ast::DataType::UnsignedInt8(_) => Ok(DataType::UInt64),
            ast::DataType::Float(_) => Ok(DataType::Float32),
            ast::DataType::Real | ast::DataType::Float4 => Ok(DataType::Float32),
            ast::DataType::Double | ast::DataType::DoublePrecision | ast::DataType::Float8 => Ok(DataType::Float64),
            ast::DataType::Char(_)
            | ast::DataType::Varchar(_)
            | ast::DataType::Text
            | ast::DataType::String(_) => Ok(DataType::Utf8),
            ast::DataType::Timestamp(None, tz_info) => {
                let tz = if matches!(tz_info, ast::TimezoneInfo::Tz)
                    || matches!(tz_info, ast::TimezoneInfo::WithTimeZone)
                {
                    // Timestamp With Time Zone
                    // INPUT : [ast::DataType]   TimestampTz + [RuntimeConfig] Time Zone
                    // OUTPUT: [ArrowDataType] Timestamp<TimeUnit, Some(Time Zone)>
                    return Err(internal!("setting timezone unsupported"))
                } else {
                    // Timestamp Without Time zone
                    None
                };
                Ok(DataType::Timestamp(TimeUnit::Microsecond, tz))
            }
            ast::DataType::Date => Ok(DataType::Date32),
            ast::DataType::Time(None, tz_info) => {
                if matches!(tz_info, ast::TimezoneInfo::None)
                    || matches!(tz_info, ast::TimezoneInfo::WithoutTimeZone)
                {
                    Ok(DataType::Time64(TimeUnit::Nanosecond))
                } else {
                    // We dont support TIMETZ and TIME WITH TIME ZONE for now
                    Err(internal!(
                        "Unsupported SQL type {:?}",
                        sql_type
                    ))
                }
            }
            ast::DataType::Numeric(exact_number_info)
            |ast::DataType::Decimal(exact_number_info) => {
                let (precision, scale) = match *exact_number_info {
                    ast::ExactNumberInfo::None => (None, None),
                    ast::ExactNumberInfo::Precision(precision) => (Some(precision), None),
                    ast::ExactNumberInfo::PrecisionAndScale(precision, scale) => {
                        (Some(precision), Some(scale))
                    }
                };
                make_decimal_type(precision, scale)
            }
            ast::DataType::Bytea => Ok(DataType::Binary),
            // Explicitly list all other types so that if sqlparser
            // adds/changes the `ast::DataType` the compiler will tell us on upgrade
            // and avoid bugs like https://github.com/apache/arrow-datafusion/issues/3059
            ast::DataType::Int64
            | ast::DataType::Float64
            | ast::DataType::Nvarchar(_)
            | ast::DataType::JSON
            | ast::DataType::Uuid
            | ast::DataType::Binary(_)
            | ast::DataType::Bytes(_)
            | ast::DataType::Varbinary(_)
            | ast::DataType::Blob(_)
            | ast::DataType::Datetime(_)
            | ast::DataType::Interval
            | ast::DataType::Regclass
            | ast::DataType::Custom(_, _)
            | ast::DataType::Array(_)
            | ast::DataType::Enum(_)
            | ast::DataType::Set(_)
            | ast::DataType::MediumInt(_)
            | ast::DataType::UnsignedMediumInt(_)
            | ast::DataType::Character(_)
            | ast::DataType::CharacterVarying(_)
            | ast::DataType::CharVarying(_)
            | ast::DataType::CharacterLargeObject(_)
            | ast::DataType::CharLargeObject(_)
            // precision is not supported
            | ast::DataType::Timestamp(Some(_), _)
            // precision is not supported
            | ast::DataType::Time(Some(_), _)
            | ast::DataType::Dec(_)
            | ast::DataType::BigNumeric(_)
            | ast::DataType::BigDecimal(_)
            | ast::DataType::Struct(_)
            | ast::DataType::Clob(_)
            | ast::DataType::JSONB
            | ast::DataType::Unspecified
            => Err(internal!(
                "Unsupported SQL type {:?}",
                sql_type
            )),
        }
}

fn get_pg_conn_str(m: &mut StatementOptions) -> Result<String> {
    let conn = match m.remove_optional("connection_string")? {
        Some(conn_str) => PostgresDbConnection::ConnectionString(conn_str),
        None => {
            let host = m.remove_required("host")?;
            let port = m.remove_optional("port")?;
            let user = m.remove_required("user")?;
            let password = m.remove_optional("password")?;
            let database = m.remove_required("database")?;
            PostgresDbConnection::Parameters {
                host,
                port,
                user,
                password,
                database,
            }
        }
    };

    Ok(conn.connection_string())
}

fn get_mysql_conn_str(m: &mut StatementOptions) -> Result<String> {
    let conn = match m.remove_optional("connection_string")? {
        Some(conn_str) => MysqlDbConnection::ConnectionString(conn_str),
        None => {
            let host = m.remove_required("host")?;
            let port = m.remove_optional("port")?;
            let user = m.remove_required("user")?;
            let password = m.remove_optional("password")?;
            let database = m.remove_required("database")?;
            MysqlDbConnection::Parameters {
                host,
                port,
                user,
                password,
                database,
            }
        }
    };

    Ok(conn.connection_string())
}

fn get_mongodb_conn_str(m: &mut StatementOptions) -> Result<String> {
    let conn = match m.remove_optional("connection_string")? {
        Some(conn_str) => MongoDbConnection::ConnectionString(conn_str),
        None => {
            let protocol = m.remove_optional("protocol")?.unwrap_or_default();
            let host = m.remove_required("host")?;
            let port = m.remove_optional("port")?;
            let user = m.remove_required("user")?;
            let password = m.remove_optional("password")?;
            MongoDbConnection::Parameters {
                protocol,
                host,
                port,
                user,
                password,
            }
        }
    };

    Ok(conn.connection_string())
}

fn get_ssh_conn_str(m: &mut StatementOptions) -> Result<String> {
    let conn = match m.remove_optional("connection_string")? {
        Some(conn_str) => SshConnection::ConnectionString(conn_str),
        None => {
            let host = m.remove_required("host")?;
            let port = m.remove_optional("port")?;
            let user = m.remove_required("user")?;
            SshConnection::Parameters(SshConnectionParameters { host, port, user })
        }
    };
    Ok(conn.connection_string())
}

/// Update storage options with the provided credentials object contents
fn storage_options_with_credentials(
    storage_options: &mut StorageOptions,
    creds: CredentialsOptions,
) {
    match creds {
        CredentialsOptions::Debug(_) => {}  // Nothing to do here
        CredentialsOptions::OpenAI(_) => {} // Nothing to do here. OpenAI is not a storage backend
        CredentialsOptions::Gcp(creds) => {
            storage_options.inner.insert(
                GoogleConfigKey::ServiceAccountKey.as_ref().to_string(),
                creds.service_account_key,
            );
        }
        CredentialsOptions::Aws(creds) => {
            storage_options.inner.insert(
                AmazonS3ConfigKey::AccessKeyId.as_ref().to_string(),
                creds.access_key_id,
            );
            storage_options.inner.insert(
                AmazonS3ConfigKey::SecretAccessKey.as_ref().to_string(),
                creds.secret_access_key,
            );
        }
        CredentialsOptions::Azure(creds) => {
            storage_options.inner.insert(
                AzureConfigKey::AccountName.as_ref().to_string(),
                creds.account_name,
            );
            storage_options.inner.insert(
                AzureConfigKey::AccessKey.as_ref().to_string(),
                creds.access_key,
            );
        }
    }
}

/// Returns a validated `DataType` for the specified precision and
/// scale
fn make_decimal_type(precision: Option<u64>, scale: Option<u64>) -> Result<DataType> {
    // postgres like behavior
    let (precision, scale) = match (precision, scale) {
        (Some(p), Some(s)) => (p as u8, s as i8),
        (Some(p), None) => (p as u8, 0),
        (None, Some(_)) => {
            return Err(internal!("Cannot specify only scale for decimal data type",))
        }
        (None, None) => (DECIMAL128_MAX_PRECISION, DECIMAL_DEFAULT_SCALE),
    };

    // Arrow decimal is i128 meaning 38 maximum decimal digits
    if precision == 0 || precision > DECIMAL128_MAX_PRECISION || scale.unsigned_abs() > precision {
        Err(internal!(
            "Decimal(precision = {}, scale = {}) should satisfy `0 < precision <= 38`, and `scale <= precision`.",
            precision, scale
        ))
    } else {
        Ok(DataType::Decimal128(precision, scale))
    }
}

/// If the "SHOW ..." statement equivalent to "SHOW TRANSACTION ISOLATION
/// LEVEL", return the variable for which to show the value.
fn is_show_transaction_isolation_level(variable: &[String]) -> bool {
    const TRANSACTION_ISOLATION_LEVEL_STMT: [&str; 3] = ["transaction", "isolation", "level"];
    variable.iter().eq(TRANSACTION_ISOLATION_LEVEL_STMT.iter())
}
