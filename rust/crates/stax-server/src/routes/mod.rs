//! The 34 route-module slots, in `server.py`'s `include_router` order.
//!
//! `python-legacy: server.py` composes its app with 34 consecutive
//! `app.include_router(...)` calls (the spec said 12; DRIFT-1 in
//! `rust/TASKS-RS.md` measured 34). That order is reproduced here literally,
//! module for module, because it is the *only* thing that disambiguates two
//! routers claiming the same path: Starlette matches routes in registration
//! order and serves the first hit. axum instead panics on a duplicate
//! method+path, so the order is currently belt-and-braces — but the day a batch
//! ports two modules that overlap, the panic is the good outcome and this list
//! is the record of who Python would have picked.
//!
//! **The fan-out contract.** One module here per `routes/*.py`, each exposing
//! exactly `pub fn register(Router<AppState>) -> Router<AppState>`. Every slot
//! already exists and is already wired, so an endpoint batch edits *only* the
//! file for the Python module it is porting — never this file, never another
//! batch's file. Nothing below needs to change again.

use axum::Router;

use crate::state::AppState;

pub mod agent_teams;
pub mod benchmark;
pub mod bookmarks;
pub mod budgets;
pub mod cfg;
pub mod commands;
pub mod compare;
pub mod context_budget;
pub mod context_replay;
pub mod cost;
pub mod data;
pub mod etl;
pub mod export;
pub mod forks;
pub mod live;
pub mod meta_agent;
pub mod misc;
pub mod optimize;
pub mod patterns;
pub mod plan;
pub mod playback;
pub mod pricing;
pub mod projects;
pub mod qa;
pub mod quality;
pub mod search;
pub mod sessions;
pub mod static_analysis;
pub mod sync;
pub mod tags;
pub mod webhooks;
pub mod whatif;
pub mod worktrees;
pub mod yield_route;

/// Mount all 34 route modules, in `include_router` order.
///
/// Unported modules return the router untouched, so this is safe to call at any
/// point in the campaign: the app is exactly as complete as the slots are.
pub fn register_all(router: Router<AppState>) -> Router<AppState> {
    let router = projects::register(router);
    let router = data::register(router);
    let router = cost::register(router);
    let router = commands::register(router);
    let router = sessions::register(router);
    let router = search::register(router);
    let router = qa::register(router);
    let router = tags::register(router);
    let router = bookmarks::register(router);
    let router = misc::register(router);
    let router = export::register(router);
    let router = optimize::register(router);
    let router = plan::register(router);
    let router = compare::register(router);
    let router = yield_route::register(router);
    let router = context_budget::register(router);
    let router = context_replay::register(router);
    let router = cfg::register(router);
    let router = etl::register(router);
    let router = agent_teams::register(router);
    let router = playback::register(router);
    let router = meta_agent::register(router);
    let router = live::register(router);
    let router = webhooks::register(router);
    let router = static_analysis::register(router);
    let router = quality::register(router);
    let router = pricing::register(router);
    let router = budgets::register(router);
    let router = whatif::register(router);
    let router = forks::register(router);
    let router = benchmark::register(router);
    let router = patterns::register(router);
    let router = worktrees::register(router);
    sync::register(router)
}
