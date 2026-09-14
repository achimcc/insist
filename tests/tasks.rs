use insist::tasks::{run_tasks, Tasks};

fn forever() -> impl std::future::Future<Output = ()> + Send + 'static {
    std::future::pending()
}

#[tokio::test]
async fn a_task_that_returns_ends_the_run_with_an_error_naming_it() {
    let mut tasks = Tasks::default();
    tasks.spawn("reconciliation", forever());
    tasks.spawn("tick", async {});
    let server = std::future::pending::<std::io::Result<()>>();
    let err = run_tasks(tasks, server)
        .await
        .expect_err("a background task that ends must end the process with an error");
    let text = format!("{err:#}");
    assert!(text.contains("tick"), "{text}");
    assert!(!text.contains("reconciliation"), "{text}");
}

#[tokio::test]
async fn a_task_that_panics_ends_the_run_with_an_error_naming_it() {
    let mut tasks = Tasks::default();
    tasks.spawn("tick", forever());
    tasks.spawn("acknowledgement stream", async { panic!("boom") });
    let server = std::future::pending::<std::io::Result<()>>();
    let err = run_tasks(tasks, server)
        .await
        .expect_err("a panic must not be swallowed");
    let text = format!("{err:#}");
    assert!(text.contains("acknowledgement stream"), "{text}");
    assert!(text.contains("panicked"), "{text}");
}

#[tokio::test]
async fn a_server_that_shuts_down_cleanly_ends_the_run_cleanly() {
    let mut tasks = Tasks::default();
    tasks.spawn("tick", forever());
    let server = async { Ok::<(), std::io::Error>(()) };
    run_tasks(tasks, server).await.unwrap();
}

#[tokio::test]
async fn a_server_error_is_an_error() {
    let mut tasks = Tasks::default();
    tasks.spawn("tick", forever());
    let server = async { Err::<(), _>(std::io::Error::other("accept failed")) };
    assert!(run_tasks(tasks, server).await.is_err());
}
