/// Cache manager.
pub mod manager;
/// Remote cache.
pub mod remote;

pub use manager::{
    get_cached_url, put_hybrid_cache, rewrite_base_tag, spawn_fetch_cache_interceptor,
    spawn_response_cache_listener, BasicCachePolicy, CacheStrategy,
};
