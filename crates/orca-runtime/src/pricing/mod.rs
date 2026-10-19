//! Cost estimates at DeepSeek's prices: the built-in table and its
//! time-of-use rule (`rates`), the process-wide price book with its cache
//! (`book`), and the daily refresh from models.dev (`refresh`).

pub mod book;
pub mod rates;
pub mod refresh;
