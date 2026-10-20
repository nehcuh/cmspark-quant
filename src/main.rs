mod models;
mod sandbox;
mod core;

// Existing dashboard prototype remains; modules are available for next steps.
// (Full UI integration will consume the models & orchestrator.)

fn main() {
    println!("cmspark-quant skeleton ready");
    println!("Key interfaces:");
    println!("  - SandboxExecutor trait");
    println!("  - Strategy state machine");
    println!("  - AuditEvent");
    println!("  - Orchestrator");
}
