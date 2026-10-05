//! The standalone JSONL agent binary: `agent/minimal_agent.py`'s entry point.

fn main() -> anyhow::Result<()> {
    agent_observer::agent::minimal_agent::run()
}
