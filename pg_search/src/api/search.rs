// Copyright (c) 2023-2024 Retake, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use crate::env::needs_commit;
use crate::index::state::SearchState;
use crate::postgres::utils::VisibilityChecker;
use crate::schema::SearchConfig;
use crate::writer::{Client, WriterDirectory, WriterRequest};
use crate::{globals::WriterGlobal, index::SearchIndex};
use pgrx::pg_sys::FunctionCallInfo;
use pgrx::{prelude::TableIterator, *};
use std::sync::{Arc, Mutex};
use tantivy::TantivyDocument;

const DEFAULT_SNIPPET_PREFIX: &str = "<b>";
const DEFAULT_SNIPPET_POSTFIX: &str = "</b>";

#[pg_extern(name = "rank_bm25")]
pub fn rank_bm25(_key: AnyElement, _alias: default!(Option<String>, "NULL")) -> f32 {
    panic!("This function has been deprecated in favor of `score_bm25`");
}

#[pg_extern]
pub fn highlight(
    _key: AnyElement,
    _field: &str,
    _prefix: default!(Option<String>, "NULL"),
    _postfix: default!(Option<String>, "NULL"),
    _max_num_chars: default!(Option<i32>, "NULL"),
    _alias: default!(Option<String>, "NULL"),
) -> String {
    panic!("This function has been deprecated in favor of `snippet`");
}

#[pg_extern]
pub fn minmax_bm25(
    _config_json: JsonB,
    _key_type_dummy: Option<AnyElement>, // This ensures that postgres knows what the return type is
    _key_oid: pgrx::pg_sys::Oid, // Have to pass oid as well because the dummy above will always by None
) -> TableIterator<'static, (name!(id, AnyElement), name!(rank_bm25, f32))> {
    panic!("`minmax_bm25` has been deprecated");
}

/// # Safety
///
/// This function is unsafe as it cannot guarantee that the provided `fcinfo` argument is valid,
/// specifically its `.flinfo.fn_mcxt` field.  This is your responsibility.
///
/// In practice, it always will be valid as Postgres sets that properly when it calls us
#[pg_extern]
unsafe fn score_bm25(
    config_json: JsonB,
    _key_type_dummy: Option<AnyElement>, // This ensures that postgres knows what the return type is
    key_oid: pgrx::pg_sys::Oid, // Have to pass oid as well because the dummy above will always by None
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(id, AnyElement), name!(score_bm25, f32))> {
    let JsonB(search_config_json) = config_json;
    let search_config: SearchConfig =
        serde_json::from_value(search_config_json.clone()).expect("could not parse search config");
    let directory = WriterDirectory::from_index_oid(search_config.index_oid);
    let search_index = SearchIndex::from_cache(&directory, &search_config.uuid)
        .unwrap_or_else(|err| panic!("error loading index from directory: {err}"));

    let writer_client = WriterGlobal::client();
    let scan_state = unsafe {
        // SAFETY:  caller has asserted that `fcinfo` is valid for this function
        create_and_leak_scan_state(fcinfo, &search_config, search_index, &writer_client)
    };
    let mut vischeck = VisibilityChecker::new(search_config.table_oid.into());

    let top_docs = scan_state
        .search(SearchIndex::executor())
        .filter(move |(scored, _)| vischeck.ctid_satisfies_snapshot(scored.ctid))
        .map(move |(scored, _)| {
            let key = unsafe {
                datum::AnyElement::from_polymorphic_datum(
                    scored
                        .key
                        .try_into_datum(PgOid::from_untagged(key_oid))
                        .expect("failed to convert key_field to datum"),
                    false,
                    key_oid,
                )
                .expect("null found in key_field")
            };

            (key, scored.bm25)
        });

    TableIterator::new(top_docs)
}

/// # Safety
///
/// This function is unsafe as it cannot guarantee that the provided `fcinfo` argument is valid,
/// specifically its `.flinfo.fn_mcxt` field.  This is your responsibility.
///
/// In practice, it always will be valid as Postgres sets that properly when it calls us
#[pg_extern]
unsafe fn snippet(
    config_json: JsonB,
    _key_type_dummy: Option<AnyElement>, // This ensures that postgres knows what the return type is
    key_oid: pgrx::pg_sys::Oid, // Have to pass oid as well because the dummy above will always by None
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<
    'static,
    (
        name!(id, AnyElement),
        name!(snippet, String),
        name!(score_bm25, f32),
    ),
> {
    let JsonB(search_config_json) = config_json;
    let search_config: SearchConfig =
        serde_json::from_value(search_config_json.clone()).expect("could not parse search config");
    let directory = WriterDirectory::from_index_oid(search_config.index_oid);
    let search_index = SearchIndex::from_cache(&directory, &search_config.uuid)
        .unwrap_or_else(|err| panic!("error loading index from directory: {err}"));

    let writer_client = WriterGlobal::client();
    let scan_state = unsafe {
        // SAFETY:  caller has asserted that `fcinfo` is valid for this function
        create_and_leak_scan_state(fcinfo, &search_config, search_index, &writer_client)
    };
    let mut vischeck = VisibilityChecker::new(search_config.table_oid.into());

    let highlight_field = search_config
        .highlight_field
        .expect("highlight_field is required");
    let mut snippet_generator = scan_state.snippet_generator(&highlight_field);
    if let Some(max_num_chars) = search_config.max_num_chars {
        snippet_generator.set_max_num_chars(max_num_chars)
    }

    let top_docs = scan_state
        .search(SearchIndex::executor())
        .filter(move |(scored, _)| vischeck.ctid_satisfies_snapshot(scored.ctid))
        .map(move |(scored, doc_address)| {
            let key = unsafe {
                datum::AnyElement::from_polymorphic_datum(
                    scored
                        .key
                        .try_into_datum(PgOid::from_untagged(key_oid))
                        .expect("failed to convert key_field to datum"),
                    false,
                    key_oid,
                )
                .expect("null found in key_field")
            };

            let doc: TantivyDocument = scan_state
                .searcher
                .doc(doc_address)
                .expect("could not find document in searcher");

            let mut snippet = snippet_generator.snippet_from_doc(&doc);
            snippet.set_snippet_prefix_postfix(
                &search_config
                    .prefix
                    .clone()
                    .unwrap_or(DEFAULT_SNIPPET_PREFIX.to_string()),
                &search_config
                    .postfix
                    .clone()
                    .unwrap_or(DEFAULT_SNIPPET_POSTFIX.to_string()),
            );

            (key, snippet.to_html(), scored.bm25)
        });

    TableIterator::new(top_docs)
}

pub fn drop_bm25_internal(index_oid: pg_sys::Oid) {
    // We need to receive the index_name as an argument here, because PGRX has
    // some limitations around passing OID / u32 as a pg_extern parameter:
    // https://github.com/pgcentralfoundation/pgrx/issues/1536

    let writer_client = WriterGlobal::client();

    // Drop the Tantivy data directory.
    SearchIndex::drop_index(&writer_client, index_oid.as_u32())
        .unwrap_or_else(|err| panic!("error dropping index with OID {index_oid:?}: {err:?}"));
}

/// # Safety
///
/// This function is unsafe as it cannot guarantee that the provided `fcinfo` argument is valid,
/// specifically its `.flinfo.fn_mcxt` field.  This is your responsibility.
///
/// In practice, it always will be valid as Postgres sets that properly when it calls us
unsafe fn create_and_leak_scan_state(
    fcinfo: FunctionCallInfo,
    search_config: &SearchConfig,
    search_index: &mut SearchIndex,
    writer_client: &Arc<Mutex<Client<WriterRequest>>>,
) -> &'static SearchState {
    // after instantiating the `SearchState`, we leak it to the MemoryContext governing this
    // function call.  This function is a SRF, and all calls to this function will have the
    // same MemoryContext.
    //
    // Leaking the scan state allows us to avoid a `.collect::<Vec<_>>()` on the search results
    // of `top_docs` down below
    let scan_state = search_index
        .search_state(
            writer_client,
            search_config,
            needs_commit(search_config.index_oid),
        )
        .expect("could not get scan state");

    unsafe {
        // SAFETY:  `fcinfo` and `fcinfo.flinfo` are provided to us by Postgres and are always valid
        // pointers when we're called by Postgres.  When somewhere else in Rust calls us, it's up
        // to the caller to pass a proper `pg_sys::FunctionCallInfo`
        let scan_state =
            PgMemoryContexts::For((*(*fcinfo).flinfo).fn_mcxt).leak_and_drop_on_delete(scan_state);

        // SAFETY:  scan_state is a valid pointer, provided by `leak_and_drop_on_delete()`, and
        // effectively now lives in the `'static` lifetime
        &*scan_state
    }
}
