use std::{collections::HashMap, fs::File, sync::Arc};

use icelake::{
    catalog::{
        Catalog, OperatorArgs, RestCatalog, StorageCatalog, OP_ARGS_ACCESS_KEY,
        OP_ARGS_ACCESS_SECRET, OP_ARGS_BUCKET, OP_ARGS_ENDPOINT, OP_ARGS_REGION, OP_ARGS_ROOT,
    },
    transaction::Transaction,
    Table,
};
use opendal::Scheme;

mod utils;
use tokio::runtime::Builder;
pub use utils::*;

use libtest_mimic::{Arguments, Trial};

pub struct TestFixture {
    docker_compose: DockerCompose,
    poetry: Poetry,
    catalog: String,

    test_case: TestCase,
}

impl TestFixture {
    pub fn new(
        docker_compose: DockerCompose,
        poetry: Poetry,
        toml_file: String,
        catalog: String,
    ) -> Self {
        let toml_file_path = format!(
            "{}/../testdata/toml/{}",
            env!("CARGO_MANIFEST_DIR"),
            toml_file
        );
        let test_case = TestCase::parse(File::open(toml_file_path).unwrap());
        Self {
            docker_compose,
            poetry,
            test_case,
            catalog,
        }
    }

    fn init_table_with_spark(&self) {
        let args = vec![
            "-s".to_string(),
            self.spark_connect_url(),
            "--sql".to_string(),
        ];
        let args: Vec<String> = args
            .into_iter()
            .chain(self.test_case.init_sqls.clone())
            .collect();
        self.poetry.run_file(
            "init.py",
            args,
            format!("Init {} with spark", self.test_case.table_name),
        )
    }

    fn check_table_with_spark(&self) {
        for check_sqls in &self.test_case.query_sql {
            self.poetry.run_file(
                "check.py",
                [
                    "-s",
                    &self.spark_connect_url(),
                    "-q1",
                    check_sqls[0].as_str(),
                    "-q2",
                    check_sqls[1].as_str(),
                ],
                format!("Check {}", check_sqls[0].as_str()),
            )
        }
    }

    fn spark_connect_url(&self) -> String {
        format!(
            "sc://{}:{}",
            self.docker_compose.get_container_ip("spark"),
            SPARK_CONNECT_SERVER_PORT
        )
    }

    pub async fn create_icelake_table(&self) -> Table {
        match self.catalog.as_str() {
            "storage" => self.create_icelake_table_with_storage_catalog().await,
            "rest" => self.create_icelake_table_with_rest_catalog().await,
            _ => panic!("Unsupported catalog: {}", self.catalog),
        }
    }

    async fn create_icelake_table_with_storage_catalog(&self) -> Table {
        let op_args = OperatorArgs::builder(Scheme::S3)
            .with_arg(OP_ARGS_ROOT, self.test_case.warehouse_root.clone())
            .with_arg(OP_ARGS_BUCKET, "icebergdata")
            .with_arg(
                OP_ARGS_ENDPOINT,
                format!(
                    "http://{}:{}",
                    self.docker_compose.get_container_ip("minio"),
                    MINIO_DATA_PORT
                ),
            )
            .with_arg(OP_ARGS_REGION, "us-east-1")
            .with_arg(OP_ARGS_ACCESS_KEY, "admin")
            .with_arg(OP_ARGS_ACCESS_SECRET, "password")
            .build();

        let catalog = Arc::new(StorageCatalog::open(op_args).await.unwrap());

        catalog
            .load_table(&self.test_case.table_name)
            .await
            .unwrap()
    }

    async fn create_icelake_table_with_rest_catalog(&self) -> Table {
        let config: HashMap<String, String> = HashMap::from([
            (
                "uri",
                format!(
                    "http://{}:{REST_CATALOG_PORT}",
                    self.docker_compose.get_container_ip("rest")
                ),
            ),
            (
                "table.io.root",
                format!(
                    "{}/{}/{}",
                    self.test_case.warehouse_root.clone(),
                    self.test_case.table_name.namespace,
                    self.test_case.table_name.name,
                ),
            ),
            ("table.io.bucket", "icebergdata".to_string()),
            (
                "table.io.endpoint",
                format!(
                    "http://{}:{}",
                    self.docker_compose.get_container_ip("minio"),
                    MINIO_DATA_PORT
                ),
            ),
            ("table.io.region", "us-east-1".to_string()),
            ("table.io.access_key_id", "admin".to_string()),
            ("table.io.secret_access_key", "password".to_string()),
        ])
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let catalog = Arc::new(RestCatalog::new(&self.catalog, config).await.unwrap());

        catalog
            .load_table(&self.test_case.table_name)
            .await
            .unwrap()
    }

    pub async fn write_data_with_icelake(&mut self) {
        let mut table = self.create_icelake_table().await;
        log::info!(
            "Real path of table is: {}",
            table.current_table_metadata().location
        );

        let records = &self.test_case.write_date;

        let mut task_writer = table.task_writer().await.unwrap();

        for record_batch in records {
            log::info!(
                "Insert record batch with {} records using icelake.",
                record_batch.num_rows()
            );
            task_writer.write(record_batch).await.unwrap();
        }

        let result = task_writer.close().await.unwrap();
        log::debug!("Insert {} data files: {:?}", result.len(), result);

        // Commit table transaction
        {
            let mut tx = Transaction::new(&mut table);
            tx.append_file(result);
            tx.commit().await.unwrap();
        }
    }

    async fn run(mut self) {
        self.init_table_with_spark();
        self.write_data_with_icelake().await;
        self.check_table_with_spark();
    }

    pub fn block_run(self) {
        let rt = Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name(self.docker_compose.project_name())
            .build()
            .unwrap();

        rt.block_on(async { self.run().await })
    }
}

fn create_test_fixture(project_name: &str, toml_file: &str, catalog: &str) -> TestFixture {
    set_up();

    let docker_compose = match catalog {
        "storage" => DockerCompose::new(project_name, "iceberg-fs"),
        "rest" => DockerCompose::new(project_name, "iceberg-rest"),
        _ => panic!("Unrecognized catalog : {catalog}"),
    };
    let poetry = Poetry::new(format!("{}/../testdata/python", env!("CARGO_MANIFEST_DIR")));

    docker_compose.run();

    TestFixture::new(
        docker_compose,
        poetry,
        toml_file.to_string(),
        catalog.to_string(),
    )
}

fn main() {
    // Parse command line arguments
    let args = Arguments::from_args();

    let catalogs = vec!["storage", "rest"];
    let test_cases = vec![
        "no_partition_test.toml",
        "partition_identity_test.toml",
        "partition_year_test.toml",
        "partition_month_test.toml",
        "partition_day_test.toml",
        "partition_hour_test.toml",
        "partition_hash_test.toml",
        "partition_truncate_test.toml",
    ];

    let mut tests = Vec::with_capacity(16);
    for catalog in &catalogs {
        for test_case in &test_cases {
            let test_name = &normalize_test_name(format!(
                "{}_test_insert_{test_case}_with_{catalog}_catalog",
                module_path!()
            ));

            let fixture = create_test_fixture(test_name, test_case, catalog);
            tests.push(Trial::test(test_name, move || {
                fixture.block_run();
                Ok(())
            }));
        }
    }

    // Run all tests and exit the application appropriatly.
    libtest_mimic::run(&args, tests).exit();
}
