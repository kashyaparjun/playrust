mod support;

use std::fs;

use playrust::report::FlowStatus;
use support::{FixtureServer, assert_success, playrust, read_report};

const HTML: &str = r#"<!doctype html><body><main id="list"></main><script>
let page = 0;
const list = document.querySelector('#list');
const render = () => list.replaceChildren(...Array.from({ length: 8 }, (_, index) => {
  const item = document.createElement('p');
  item.textContent = page === 4 && index === 7 ? 'Virtual item 39' : `Virtual item ${page * 8 + index}`;
  return item;
}));
addEventListener('wheel', event => { if (event.deltaY > 0) { page++; render(); } });
render();
</script></body>"#;

#[test]
#[ignore = "requires PLAYRUST_CHROME to point to the pinned Chrome executable"]
fn scrolls_a_virtualized_list_until_the_target_exists() {
    let server = FixtureServer::start(&[("/", "text/html", HTML)]);
    let directory = tempfile::tempdir().expect("create E2E directory");
    let flow = directory.path().join("scroll-until-visible.yaml");
    let artifacts = directory.path().join("artifacts");
    fs::write(
        &flow,
        format!(
            "version: 1\nname: scroll-until-visible\nbase_url: {}\nsettings: {{ video: off }}\nsteps:\n  - open: /\n  - timeout: 3s\n    scroll_until_visible: {{ target: {{ text: 'Virtual item 39' }}, y: 300 }}\n",
            server.url()
        ),
    )
    .expect("write flow");

    let run = playrust(
        &[
            "run",
            flow.to_str().unwrap(),
            "--artifacts",
            artifacts.to_str().unwrap(),
        ],
        &[],
    );
    server.shutdown();
    assert_success("run", &run);
    assert_eq!(read_report(&artifacts).flows[0].status, FlowStatus::Passed);
}
