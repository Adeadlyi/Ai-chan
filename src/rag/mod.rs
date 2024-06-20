use self::bm25::*;
use self::loader::*;
use self::splitter::*;

use crate::client::*;
use crate::config::*;
use crate::utils::*;

mod bm25;
mod loader;
mod splitter;

use anyhow::bail;
use anyhow::{anyhow, Context, Result};
use hnsw_rs::prelude::*;
use indexmap::IndexMap;
use inquire::{required, validator::Validation, Select, Text};
use path_absolutize::Absolutize;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, fmt::Debug, io::BufReader, path::Path};
use tokio::sync::mpsc;

pub struct Rag {
    client: Box<dyn Client>,
    name: String,
    path: String,
    model: Model,
    hnsw: Hnsw<'static, f32, DistCosine>,
    bm25: BM25<VectorID>,
    data: RagData,
}

impl Debug for Rag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rag")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("model", &self.model)
            .field("data", &self.data)
            .finish()
    }
}

impl Rag {
    pub async fn init(
        config: &GlobalConfig,
        name: &str,
        save_path: &Path,
        doc_paths: &[String],
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        debug!("init rag: {name}");
        let (model, chunk_size, chunk_overlap) = Self::config(config)?;
        let data = RagData::new(&model.id(), chunk_size, chunk_overlap);
        let mut rag = Self::create(config, name, save_path, data)?;
        let mut paths = doc_paths.to_vec();
        if paths.is_empty() {
            paths = add_doc_paths()?;
        };
        debug!("doc paths: {paths:?}");
        let (stop_spinner_tx, set_spinner_message_tx) = run_spinner("Starting").await;
        tokio::select! {
            ret = rag.add_paths(&paths, Some(set_spinner_message_tx)) => {
                let _ = stop_spinner_tx.send(());
                ret?;
            }
            _ = watch_abort_signal(abort_signal) => {
                let _ = stop_spinner_tx.send(());
                bail!("Aborted!")
            },
        };
        if !rag.is_temp() {
            rag.save(save_path)?;
            println!("✨ Saved rag to '{}'", save_path.display());
        }
        Ok(rag)
    }

    pub fn load(config: &GlobalConfig, name: &str, path: &Path) -> Result<Self> {
        let err = || format!("Failed to load rag '{name}'");
        let file = std::fs::File::open(path).with_context(err)?;
        let reader = BufReader::new(file);
        let data: RagData = bincode::deserialize_from(reader).with_context(err)?;
        Self::create(config, name, path, data)
    }

    pub fn create(config: &GlobalConfig, name: &str, path: &Path, data: RagData) -> Result<Self> {
        let hnsw = data.build_hnsw();
        let bm25 = data.build_bm25();
        let model = Model::retrieve_embedding(&config.read(), &data.model)?;
        let client = init_client(config, Some(model.clone()))?;
        let rag = Rag {
            client,
            name: name.to_string(),
            path: path.display().to_string(),
            data,
            model,
            hnsw,
            bm25,
        };
        Ok(rag)
    }

    pub fn config(config: &GlobalConfig) -> Result<(Model, usize, usize)> {
        let (embedding_model, chunk_size, chunk_overlap) = {
            let config = config.read();
            (
                config.rag_embedding_model.clone(),
                config.rag_chunk_size,
                config.rag_chunk_overlap,
            )
        };
        let model_id = match embedding_model {
            Some(value) => {
                println!("Select embedding model: {value}");
                value
            }
            None => {
                let models = list_embedding_models(&config.read());
                if models.is_empty() {
                    bail!("No available embedding model");
                }
                if *IS_STDOUT_TERMINAL {
                    select_embedding_model(&models)?
                } else {
                    let value = models[0].id();
                    println!("Select embedding model: {value}");
                    value
                }
            }
        };
        let model = Model::retrieve_embedding(&config.read(), &model_id)?;
        let chunk_size = match chunk_size {
            Some(value) => {
                println!("Set chunk size: {value}");
                value
            }
            None => {
                if *IS_STDOUT_TERMINAL {
                    set_chunk_size(&model)?
                } else {
                    let value = model.default_chunk_size();
                    println!("Set chunk size: {value}");
                    value
                }
            }
        };
        let chunk_overlap = match chunk_overlap {
            Some(value) => {
                println!("Set chunk overlay: {value}");
                value
            }
            None => {
                let value = chunk_size / 20;
                if *IS_STDOUT_TERMINAL {
                    set_chunk_overlay(value)?
                } else {
                    println!("Set chunk overlay: {value}");
                    value
                }
            }
        };
        Ok((model, chunk_size, chunk_overlap))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        ensure_parent_exists(path)?;
        let mut file = std::fs::File::create(path)?;
        bincode::serialize_into(&mut file, &self.data)
            .with_context(|| format!("Failed to save rag '{}'", self.name))?;
        Ok(())
    }

    pub fn export(&self) -> Result<String> {
        let files: Vec<_> = self.data.files.iter().map(|v| &v.path).collect();
        let data = json!({
            "path": self.path,
            "model": self.model.id(),
            "chunk_size": self.data.chunk_size,
            "chunk_overlap": self.data.chunk_overlap,
            "files": files,
        });
        let output = serde_yaml::to_string(&data)
            .with_context(|| format!("Unable to show info about rag '{}'", self.name))?;
        Ok(output)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_temp(&self) -> bool {
        self.name == TEMP_RAG_NAME
    }

    pub async fn search(
        &self,
        text: &str,
        top_k: usize,
        min_score_vector: f32,
        min_score_text: f32,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let (stop_spinner_tx, _) = run_spinner("Searching").await;
        let ret = tokio::select! {
            ret = self.hybird_search(text, top_k, min_score_vector, min_score_text) => {
                ret
            }
            _ = watch_abort_signal(abort_signal) => {
                bail!("Aborted!")
            },
        };
        let _ = stop_spinner_tx.send(());
        let output = ret?.join("\n\n");
        Ok(output)
    }

    pub async fn add_paths<T: AsRef<Path>>(
        &mut self,
        paths: &[T],
        progress_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> Result<()> {
        // List files
        let mut file_paths = vec![];
        progress(&progress_tx, "Listing paths".into());
        for path in paths {
            let path = path
                .as_ref()
                .absolutize()
                .with_context(|| anyhow!("Invalid path '{}'", path.as_ref().display()))?;
            let path_str = path.display().to_string();
            if self.data.files.iter().any(|v| v.path == path_str) {
                continue;
            }
            let (path_str, suffixes) = parse_glob(&path_str)?;
            let suffixes = if suffixes.is_empty() {
                None
            } else {
                Some(&suffixes)
            };
            list_files(&mut file_paths, Path::new(&path_str), suffixes).await?;
        }

        // Load files
        let mut rag_files = vec![];
        let file_paths_len = file_paths.len();
        progress(&progress_tx, format!("Loading files [1/{file_paths_len}]"));
        for path in file_paths {
            let extension = Path::new(&path)
                .extension()
                .map(|v| v.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let separator = detect_separators(&extension);
            let splitter = RecursiveCharacterTextSplitter::new(
                self.data.chunk_size,
                self.data.chunk_overlap,
                &separator,
            );
            let documents = load(&path, &extension)
                .with_context(|| format!("Failed to load file at '{path}'"))?;
            let documents =
                splitter.split_documents(&documents, &SplitterChunkHeaderOptions::default());
            rag_files.push(RagFile { path, documents });
            progress(
                &progress_tx,
                format!("Loading files [{}/{file_paths_len}]", rag_files.len()),
            );
        }

        if rag_files.is_empty() {
            return Ok(());
        }

        // Convert vectors
        let mut vector_ids = vec![];
        let mut texts = vec![];
        for (file_index, file) in rag_files.iter().enumerate() {
            for (document_index, document) in file.documents.iter().enumerate() {
                vector_ids.push(combine_vector_id(file_index, document_index));
                texts.push(document.page_content.clone())
            }
        }

        let embeddings_data = EmbeddingsData::new(texts, false);
        let embeddings = self
            .create_embeddings(embeddings_data, progress_tx.clone())
            .await?;

        self.data.add(rag_files, vector_ids, embeddings);
        progress(&progress_tx, "Building vector store".into());
        self.hnsw = self.data.build_hnsw();

        Ok(())
    }

    async fn hybird_search(
        &self,
        query: &str,
        top_k: usize,
        min_score_vector: f32,
        min_score_text: f32,
    ) -> Result<Vec<String>> {
        let (vector_search_result, text_search_result) = tokio::join!(
            self.vector_search(query, top_k, min_score_vector),
            self.text_search(query, top_k, min_score_text)
        );
        let vector_search_ids = vector_search_result?;
        let text_search_ids = text_search_result?;
        let ids = reciprocal_rank_fusion(vector_search_ids, text_search_ids, 1.0, 1.0, top_k);
        let output: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                let (file_index, document_index) = split_vector_id(id);
                let file = self.data.files.get(file_index)?;
                let document = file.documents.get(document_index)?;
                Some(document.page_content.clone())
            })
            .collect();
        Ok(output)
    }

    async fn vector_search(
        &self,
        query: &str,
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<VectorID>> {
        let splitter = RecursiveCharacterTextSplitter::new(
            self.data.chunk_size,
            self.data.chunk_overlap,
            &DEFAULT_SEPARATES,
        );
        let texts = splitter.split_text(query);
        let embeddings_data = EmbeddingsData::new(texts, true);
        let embeddings = self.create_embeddings(embeddings_data, None).await?;
        let output = self
            .hnsw
            .parallel_search(&embeddings, top_k, 30)
            .into_iter()
            .flat_map(|list| {
                list.into_iter()
                    .filter_map(|v| {
                        if v.distance < min_score {
                            return None;
                        }
                        Some(v.d_id)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(output)
    }

    async fn text_search(
        &self,
        query: &str,
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<VectorID>> {
        let output = self.bm25.search(query, top_k, Some(min_score as f64));
        Ok(output)
    }

    async fn create_embeddings(
        &self,
        data: EmbeddingsData,
        progress_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> Result<EmbeddingsOutput> {
        let EmbeddingsData { texts, query } = data;
        let mut output = vec![];
        let chunks = texts.chunks(self.model.max_concurrent_chunks());
        let chunks_len = chunks.len();
        progress(
            &progress_tx,
            format!("Creating embeddings [1/{chunks_len}]"),
        );
        for (index, texts) in chunks.enumerate() {
            let chunk_data = EmbeddingsData {
                texts: texts.to_vec(),
                query,
            };
            let chunk_output = self
                .client
                .embeddings(chunk_data)
                .await
                .context("Failed to create embedding")?;
            output.extend(chunk_output);
            progress(
                &progress_tx,
                format!("Creating embeddings [{}/{chunks_len}]", index + 1),
            );
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagData {
    pub model: String,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub files: Vec<RagFile>,
    pub vectors: IndexMap<VectorID, Vec<f32>>,
}

impl RagData {
    pub fn new(model: &str, chunk_size: usize, chunk_overlap: usize) -> Self {
        Self {
            model: model.to_string(),
            chunk_size,
            chunk_overlap,
            files: Default::default(),
            vectors: Default::default(),
        }
    }

    pub fn add(
        &mut self,
        files: Vec<RagFile>,
        vector_ids: Vec<VectorID>,
        embeddings: EmbeddingsOutput,
    ) {
        self.files.extend(files);
        self.vectors.extend(vector_ids.into_iter().zip(embeddings));
    }

    pub fn build_hnsw(&self) -> Hnsw<'static, f32, DistCosine> {
        let hnsw = Hnsw::new(32, self.vectors.len(), 16, 200, DistCosine {});
        let list: Vec<_> = self.vectors.iter().map(|(k, v)| (v, *k)).collect();
        hnsw.parallel_insert(&list);
        hnsw
    }

    pub fn build_bm25(&self) -> BM25<VectorID> {
        let mut corpus = vec![];
        for (file_index, file) in self.files.iter().enumerate() {
            for (document_index, document) in file.documents.iter().enumerate() {
                let id = combine_vector_id(file_index, document_index);
                corpus.push((id, document.page_content.clone()));
            }
        }
        BM25::new(corpus, BM25Options::default())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagFile {
    path: String,
    documents: Vec<RagDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagDocument {
    pub page_content: String,
    pub metadata: RagMetadata,
}

impl RagDocument {
    pub fn new<S: Into<String>>(page_content: S) -> Self {
        RagDocument {
            page_content: page_content.into(),
            metadata: IndexMap::new(),
        }
    }

    #[allow(unused)]
    pub fn with_metadata(mut self, metadata: RagMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

impl Default for RagDocument {
    fn default() -> Self {
        RagDocument {
            page_content: "".to_string(),
            metadata: IndexMap::new(),
        }
    }
}

pub type RagMetadata = IndexMap<String, String>;

pub type VectorID = usize;

pub fn combine_vector_id(file_index: usize, document_index: usize) -> VectorID {
    file_index << (usize::BITS / 2) | document_index
}

pub fn split_vector_id(value: VectorID) -> (usize, usize) {
    let low_mask = (1 << (usize::BITS / 2)) - 1;
    let low = value & low_mask;
    let high = value >> (usize::BITS / 2);
    (high, low)
}

fn select_embedding_model(models: &[&Model]) -> Result<String> {
    let model_ids: Vec<_> = models.iter().map(|v| v.id()).collect();
    let model_id = Select::new("Select embedding model:", model_ids).prompt()?;
    Ok(model_id)
}

fn set_chunk_size(model: &Model) -> Result<usize> {
    let default_value = model.default_chunk_size().to_string();
    let help_message = model
        .max_input_tokens()
        .map(|v| format!("The model's max_input_token is {v}"));

    let mut text = Text::new("Set chunk size:")
        .with_default(&default_value)
        .with_validator(move |text: &str| {
            let out = match text.parse::<usize>() {
                Ok(_) => Validation::Valid,
                Err(_) => Validation::Invalid("Must be a integer".into()),
            };
            Ok(out)
        });
    if let Some(help_message) = &help_message {
        text = text.with_help_message(help_message);
    }
    let value = text.prompt()?;
    value.parse().map_err(|_| anyhow!("Invalid chunk_size"))
}

fn set_chunk_overlay(default_value: usize) -> Result<usize> {
    let value = Text::new("Set chunk overlay:")
        .with_default(&default_value.to_string())
        .with_validator(move |text: &str| {
            let out = match text.parse::<usize>() {
                Ok(_) => Validation::Valid,
                Err(_) => Validation::Invalid("Must be a integer".into()),
            };
            Ok(out)
        })
        .prompt()?;
    value.parse().map_err(|_| anyhow!("Invalid chunk_overlay"))
}

fn add_doc_paths() -> Result<Vec<String>> {
    let text = Text::new("Add document paths:")
        .with_validator(required!("This field is required"))
        .with_help_message("e.g. file1;dir2/;dir3/**/*.md")
        .prompt()?;
    let paths = text.split(';').map(|v| v.to_string()).collect();
    Ok(paths)
}

fn progress(spinner_message_tx: &Option<mpsc::UnboundedSender<String>>, message: String) {
    if let Some(tx) = spinner_message_tx {
        let _ = tx.send(message);
    }
}

fn reciprocal_rank_fusion(
    vector_search_ids: Vec<VectorID>,
    text_search_ids: Vec<VectorID>,
    vector_search_weight: f32,
    text_search_weight: f32,
    top_k: usize,
) -> Vec<VectorID> {
    let rrf_k = top_k * 2;
    let mut map: HashMap<VectorID, f32> = HashMap::new();
    for (index, &item) in vector_search_ids.iter().enumerate() {
        *map.entry(item).or_default() +=
            (1.0 / ((rrf_k + index + 1) as f32)) * vector_search_weight;
    }
    for (index, &item) in text_search_ids.iter().enumerate() {
        *map.entry(item).or_default() += (1.0 / ((rrf_k + index + 1) as f32)) * text_search_weight;
    }
    let mut sorted_items: Vec<(VectorID, f32)> = map.into_iter().collect();
    sorted_items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    sorted_items
        .into_iter()
        .take(top_k)
        .map(|(v, _)| v)
        .collect()
}
