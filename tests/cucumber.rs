use cucumber::World;

#[derive(Debug, Default, World)]
pub struct AppWorld;

#[tokio::main]
async fn main() {
    AppWorld::run("tests/features").await;
}
