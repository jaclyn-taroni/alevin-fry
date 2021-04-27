/*
 * Copyright (c) 2020-2021 Rob Patro, Avi Srivastava, Hirak Sarkar, Dongze He, Mohsen Zakeri.
 *
 * This file is part of alevin-fry
 * (see https://github.com/COMBINE-lab/alevin-fry).
 *
 * License: 3-clause BSD, see https://opensource.org/licenses/BSD-3-Clause
 */

extern crate ahash;
extern crate bincode;
extern crate crossbeam_queue;
extern crate indicatif;
extern crate needletail;
extern crate petgraph;
extern crate serde;
extern crate slog;

use self::indicatif::{ProgressBar, ProgressStyle};
use self::petgraph::prelude::*;
#[allow(unused_imports)]
use self::slog::{crit, info, warn};
use crate as libradicl;
use crossbeam_queue::ArrayQueue;

use needletail::bitkmer::*;
use scroll::{Pread, Pwrite};
use serde_json::json;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::io::{BufReader, BufWriter};
use std::string::ToString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
//use std::ptr;

use flate2::write::GzEncoder;
use flate2::Compression;

use self::libradicl::em::{em_optimize, run_bootstrap, EmInitType};
use self::libradicl::pugutils;
use self::libradicl::schema::{EqMap, PugEdgeType, ResolutionStrategy};
use self::libradicl::utils::*;

/// Extracts the parsimonious UMI graphs (PUGs) from the
/// equivalence class map for a given cell.
/// The returned graph is a directed graph (potentially with
/// bidirected edges) where each node consists of an (equivalence
/// class, UMI ID) pair.  Note, crucially, that the UMI ID is simply
/// the rank of the UMI in the list of all distinct UMIs for this
/// equivalence class.  There is a directed edge between any pair of
/// vertices whose set of transcripts overlap and whose UMIs are within
/// a Hamming distance of 1 of each other.  If one node has more than
/// twice the frequency of the other, the edge is directed from the
/// more frequent to the less freuqent node.  Otherwise, edges are
/// added in both directions.
fn extract_graph(
    eqmap: &EqMap,
    log: &slog::Logger,
) -> petgraph::graphmap::GraphMap<(u32, u32), (), petgraph::Directed> {
    let verbose = false;
    let mut one_edit = 0u64;
    let mut zero_edit = 0u64;

    // given 2 pairs (UMI, count), determine if an edge exists
    // between them, and if so, what type.
    let mut has_edge = |x: &(u64, u32), y: &(u64, u32)| -> PugEdgeType {
        let hdist = count_diff_2_bit_packed(x.0, y.0);
        if hdist == 0 {
            zero_edit += 1;
            return PugEdgeType::BiDirected;
        }

        if hdist < 2 {
            one_edit += 1;
            if x.1 > (2 * y.1 - 1) {
                return PugEdgeType::XToY;
            } else if y.1 > (2 * x.1 - 1) {
                return PugEdgeType::YToX;
            } else {
                return PugEdgeType::BiDirected;
            }
        }
        PugEdgeType::NoEdge
    };

    let mut _bidirected = 0u64;
    let mut _unidirected = 0u64;

    let mut graph = DiGraphMap::<(u32, u32), ()>::new();
    let mut hset = vec![0u8; eqmap.num_eq_classes()];
    let mut idxvec: SmallVec<[u32; 128]> = SmallVec::new();

    // insert all of the nodes up front to avoid redundant
    // checks later.
    for eqid in 0..eqmap.num_eq_classes() {
        // get the info Vec<(UMI, frequency)>
        let eq = &eqmap.eqc_info[eqid];
        let u1 = &eq.umis;
        for (xi, _x) in u1.iter().enumerate() {
            graph.add_node((eqid as u32, xi as u32));
        }
    }

    // for every equivalence class in this cell
    for eqid in 0..eqmap.num_eq_classes() {
        if verbose && eqid % 1000 == 0 {
            print!("\rprocessed {:?} eq classes", eqid);
            io::stdout().flush().expect("Could not flush stdout");
        }

        // get the info Vec<(UMI, frequency)>
        let eq = &eqmap.eqc_info[eqid];

        // for each (umi, count) pair and its index
        let u1 = &eq.umis;
        for (xi, x) in u1.iter().enumerate() {
            // add a node
            // graph.add_node((eqid as u32, xi as u32));

            // for each (umi, freq) pair and node after this one
            for (xi2, x2) in u1.iter().enumerate().skip(xi + 1) {
                //for xi2 in (xi + 1)..u1.len() {
                // x2 is the other (umi, freq) pair
                //let x2 = &u1[xi2];

                // add a node for it
                // graph.add_node((eqid as u32, xi2 as u32));

                // determine if an edge exists between x and x2, and if so, what kind
                let et = has_edge(&x, &x2);
                // for each type of edge, add the appropriate edge in the graph
                match et {
                    PugEdgeType::BiDirected => {
                        graph.add_edge((eqid as u32, xi as u32), (eqid as u32, xi2 as u32), ());
                        graph.add_edge((eqid as u32, xi2 as u32), (eqid as u32, xi as u32), ());
                        _bidirected += 1;
                        //if multi_gene_vec[eqid] == true {
                        //    bidirected_in_multigene += 1;
                        //}
                    }
                    PugEdgeType::XToY => {
                        graph.add_edge((eqid as u32, xi as u32), (eqid as u32, xi2 as u32), ());
                        _unidirected += 1;
                        //if multi_gene_vec[eqid] == true {
                        //    unidirected_in_multigene += 1;
                        //}
                    }
                    PugEdgeType::YToX => {
                        graph.add_edge((eqid as u32, xi2 as u32), (eqid as u32, xi as u32), ());
                        _unidirected += 1;
                        //if multi_gene_vec[eqid] == true {
                        //    unidirected_in_multigene += 1;
                        //}
                    }
                    PugEdgeType::NoEdge => {}
                }
            }
        }

        //hset.clear();
        //hset.resize(eqmap.num_eq_classes(), 0u8);
        for i in &idxvec {
            hset[*i as usize] = 0u8;
        }
        let stf = idxvec.len() > 128;
        idxvec.clear();
        if stf {
            idxvec.shrink_to_fit();
        }

        // for every reference id in this eq class
        for r in eqmap.refs_for_eqc(eqid as u32) {
            // find the equivalence classes sharing this reference
            for eq2id in eqmap.eq_classes_containing(*r).iter() {
                // if eq2id <= eqid, then we already observed the relevant edges
                // when we process eq2id
                if (*eq2id as usize) <= eqid {
                    continue;
                }
                // otherwise, if we have already processed this other equivalence
                // class because it shares _another_ reference (apart from r) with
                // the current equivalence class, then skip it.
                if hset[*eq2id as usize] > 0 {
                    continue;
                }

                // recall that we processed this eq class as a neighbor of eqid
                hset[*eq2id as usize] = 1;
                idxvec.push(*eq2id as u32);
                let eq2 = &eqmap.eqc_info[*eq2id as usize];

                // compare all the umis between eqid and eq2id
                let u2 = &eq2.umis;
                for (xi, x) in u1.iter().enumerate() {
                    // Node for equiv : eqid and umi : xi
                    // graph.add_node((eqid as u32, xi as u32));

                    for (yi, y) in u2.iter().enumerate() {
                        // Node for equiv : eq2id and umi : yi
                        // graph.add_node((*eq2id as u32, yi as u32));

                        let et = has_edge(&x, &y);
                        match et {
                            PugEdgeType::BiDirected => {
                                graph.add_edge((eqid as u32, xi as u32), (*eq2id, yi as u32), ());
                                graph.add_edge((*eq2id, yi as u32), (eqid as u32, xi as u32), ());
                                _bidirected += 1;
                                //if multi_gene_vec[eqid] == true
                                //    || multi_gene_vec[*eq2id as usize] == true
                                //{
                                //    bidirected_in_multigene += 1;
                                //}
                            }
                            PugEdgeType::XToY => {
                                graph.add_edge((eqid as u32, xi as u32), (*eq2id, yi as u32), ());
                                _unidirected += 1;
                                //if multi_gene_vec[eqid] == true
                                //    || multi_gene_vec[*eq2id as usize] == true
                                //{
                                //    unidirected_in_multigene += 1;
                                //}
                            }
                            PugEdgeType::YToX => {
                                graph.add_edge((*eq2id, yi as u32), (eqid as u32, xi as u32), ());
                                _unidirected += 1;
                                //if multi_gene_vec[eqid] == true
                                //    || multi_gene_vec[*eq2id as usize] == true
                                //{
                                //    unidirected_in_multigene += 1;
                                //}
                            }
                            PugEdgeType::NoEdge => {}
                        }
                    }
                }
            }
        }
    }

    if verbose {
        info!(
            log,
            "\n\nsize of graph ({:?}, {:?})\n\n",
            graph.node_count(),
            graph.edge_count()
        );
        let total_edits = (one_edit + zero_edit) as f64;
        info!(log, "\n\n\n{}\n\n\n", one_edit as f64 / total_edits);
    }

    graph
}

type BufferedGzFile = BufWriter<GzEncoder<fs::File>>;
struct BootstrapHelper {
    bsfile: Option<BufferedGzFile>,
    mean_var_files: Option<(BufferedGzFile, BufferedGzFile)>,
}

impl BootstrapHelper {
    fn new(
        output_path: &std::path::Path,
        num_bootstraps: u32,
        summary_stat: bool,
    ) -> BootstrapHelper {
        if num_bootstraps > 0 {
            if summary_stat {
                let bootstrap_mean_path = output_path.join("bootstraps_mean.eds.gz");
                let bootstrap_var_path = output_path.join("bootstraps_var.eds.gz");
                let bt_mean_buffered = GzEncoder::new(
                    fs::File::create(bootstrap_mean_path).unwrap(),
                    Compression::default(),
                );
                let bt_var_buffered = GzEncoder::new(
                    fs::File::create(bootstrap_var_path).unwrap(),
                    Compression::default(),
                );
                BootstrapHelper {
                    bsfile: None,
                    mean_var_files: Some((
                        BufWriter::new(bt_mean_buffered),
                        BufWriter::new(bt_var_buffered),
                    )),
                }
            } else {
                let bootstrap_path = output_path.join("bootstraps.eds.gz");
                let bt_buffered = GzEncoder::new(
                    fs::File::create(bootstrap_path).unwrap(),
                    Compression::default(),
                );
                BootstrapHelper {
                    bsfile: Some(BufWriter::new(bt_buffered)),
                    mean_var_files: None,
                }
            }
        } else {
            BootstrapHelper {
                bsfile: None,
                mean_var_files: None,
            }
        }
    }
}

struct QuantOutputInfo {
    barcode_file: BufWriter<fs::File>,
    eds_file: BufWriter<GzEncoder<fs::File>>,
    feature_file: BufWriter<fs::File>,
    trimat: sprs::TriMatI<f32, u32>,
    row_index: usize,
    bootstrap_helper: BootstrapHelper, //sample_or_mean_and_var: (BufWriter<GzEncoder<fs::File>>)
}

struct EqcMap {
    // the *global* gene-level equivalence class map
    global_eqc: HashMap<Vec<u32>, u64, ahash::RandomState>,
    // the list of equivalence classes (and corresponding umi count)
    // that occurs in each cell.
    cell_level_count: Vec<(u64, u32)>,
    // a vector of tuples that contains pairs of the form
    // (row offset, number of equivalence classes in this cell)
    cell_offset: Vec<(usize, usize)>,
}

fn write_eqc_counts(
    eqid_map_lock: &Arc<Mutex<EqcMap>>,
    num_genes: usize,
    output_path: &std::path::Path,
    log: &slog::Logger,
) -> bool {
    let eqmap_deref = eqid_map_lock.lock();
    let geqmap = eqmap_deref.unwrap();
    let num_eqclasses = geqmap.global_eqc.len();

    info!(
        log,
        "Writing gene level equivalence class with {:?} classes",
        geqmap.global_eqc.len()
    );

    // the sparse matrix that will hold the equivalence class counts
    let mut eqmat = sprs::TriMatI::<f32, u32>::with_capacity(
        (geqmap.cell_offset.len(), num_eqclasses), // cells x eq-classes
        geqmap.cell_level_count.len(),             // num non-zero entries
    );

    // fill in the matrix
    let mut global_offset = 0usize;
    for (row_index, num_cell_eqs) in geqmap.cell_offset.iter() {
        let slice = global_offset..(global_offset + num_cell_eqs);
        for (eqid, umi_count) in geqmap.cell_level_count[slice].iter() {
            eqmat.add_triplet(*row_index, *eqid as usize, *umi_count as f32);
        }
        global_offset += num_cell_eqs;
    }

    // and write it to file.
    let mtx_path = output_path.join("geqc_counts.mtx");
    sprs::io::write_matrix_market(&mtx_path, &eqmat).expect("could not write geqc_counts.mtx");

    // write the sets of genes that define each eqc
    let gn_eq_path = output_path.join("gene_eqclass.txt.gz");
    let mut gn_eq_writer = BufWriter::new(GzEncoder::new(
        fs::File::create(gn_eq_path).unwrap(),
        Compression::default(),
    ));

    // number of genes
    gn_eq_writer
        .write_all(format!("{}\n", num_genes).as_bytes())
        .expect("could not write to gene_eqclass.txt.gz");

    // number of classes
    gn_eq_writer
        .write_all(format!("{}\n", num_eqclasses).as_bytes())
        .expect("could not write to gene_eqclass.txt.gz");

    // each line describes a class in terms of
    // the tab-separated tokens
    // g_1 g_2 ... g_k eqid
    for (gene_list, eqid) in geqmap.global_eqc.iter() {
        for g in gene_list.iter() {
            gn_eq_writer
                .write_all(format!("{}\t", g).as_bytes())
                .expect("could not write to gene_eqclass.txt.gz");
        }
        gn_eq_writer
            .write_all(format!("{}\n", eqid).as_bytes())
            .expect("could not write to gene_eqclass.txt.gz");
    }
    true
}

type MetaChunk = (usize, usize, u32, u32, Vec<u8>);

fn fill_work_queue<T: Read>(
    q: Arc<ArrayQueue<MetaChunk>>,
    mut br: BufReader<T>,
    num_chunks: usize,
    pbar: &ProgressBar,
) -> Result<(), Box<dyn std::error::Error>> {
    const BUFSIZE: usize = 524208;
    // the buffer that will hold our records
    let mut buf = vec![0u8; BUFSIZE];
    // the number of bytes currently packed into the chunk
    let mut cbytes = 0u32;
    // the number of records currently packed into the chunk
    let mut crec = 0u32;
    // the number of cells in the current chunk
    let mut cells_in_chunk = 0usize;
    // the offset of the first cell in this chunk
    let mut first_cell = 0usize;
    // if we had to expand the buffer already and should
    // forcibly push the current buffer onto the queue
    let mut force_push = false;
    // the number of bytes and records in the next chunk header
    let mut nbytes_chunk = 0u32;
    let mut nrec_chunk = 0u32;

    // we include the endpoint here because we will not actually
    // copy a chunk in the first iteration (since we have not yet
    // read the header, which comes at the end of the loop).
    for chunk_num in 0..=num_chunks {
        // in the first iteration we've not read a header yet
        // so we can't fill a chunk, otherwise we read the header
        // at the bottom of the previous iteration of this loop, and
        // we will fill in the buffer appropriately here.
        if chunk_num > 0 {
            // if the currenc cell (the cell whose header we read in the last iteration of
            // the loop) alone is too big for the buffer, than resize the buffer to be big enough
            if nbytes_chunk as usize > buf.len() {
                // if we had to resize the buffer to fit this cell, then make sure we push
                // immediately in the next round
                force_push = true;
                let chunk_resize = nbytes_chunk as usize + cbytes as usize;
                buf.resize(chunk_resize, 0);
            }

            // copy the data for the current chunk into the buffer
            let boffset = cbytes as usize;
            buf.pwrite::<u32>(nbytes_chunk, boffset)?;
            buf.pwrite::<u32>(nrec_chunk, boffset + 4)?;
            br.read_exact(&mut buf[(boffset + 8)..(boffset + nbytes_chunk as usize)])
                .unwrap();
            cells_in_chunk += 1;
            cbytes += nbytes_chunk;
            crec += nrec_chunk;
        }

        // in the last iteration of the loop, we will have read num_chunks headers already
        // and we are just filling up the buffer with the last cell, and there will be no more
        // headers left to read, so skip this
        if chunk_num < num_chunks {
            let (nc, nr) = libradicl::Chunk::read_header(&mut br);
            nbytes_chunk = nc;
            nrec_chunk = nr;
        }

        // determine if we should dump the current buffer to the work queue
        if force_push  // if we were told to push this chunk
           || // or if adding the next cell to this chunk would exceed the buffer size
           ((cbytes + nbytes_chunk) as usize > buf.len() && cells_in_chunk > 0)
           || // of if this was the last chunk
           chunk_num == num_chunks
        {
            // launch off these cells on the queue
            let mut bclone = (first_cell, cells_in_chunk, cbytes, crec, buf.clone());
            // keep trying until we can push this payload
            while let Err(t) = q.push(bclone) {
                bclone = t;
                // no point trying to push if the queue is full
                while q.is_full() {}
            }
            pbar.inc(cells_in_chunk as u64);

            // offset of the first cell in the next chunk
            first_cell += cells_in_chunk;
            // reset the counters
            cells_in_chunk = 0;
            cbytes = 0;
            crec = 0;
            buf.resize(BUFSIZE, 0);
            force_push = false;
        }
    }
    Ok(())
}

/// This function is the same as `fill_work_queue`, except that
/// when parsing the input file, it ignores (i.e. does not enqueue)
/// any cell whose barcode is not in `keep_set`.
fn fill_work_queue_filtered<T: Read>(
    keep_set: HashSet<u64, ahash::RandomState>,
    rl_tags: &libradicl::TagSection,
    q: Arc<ArrayQueue<MetaChunk>>,
    mut br: BufReader<T>,
    num_chunks: usize,
    pbar: &ProgressBar,
) -> Result<(), Box<dyn std::error::Error>> {
    let bct = rl_tags.tags[0].typeid;
    let umit = rl_tags.tags[1].typeid;
    let bc_type = libradicl::decode_int_type_tag(bct).expect("unsupported barcode type id.");
    let umi_type = libradicl::decode_int_type_tag(umit).expect("unsupported umi type id.");

    const BUFSIZE: usize = 524208;
    // the buffer that will hold our records
    let mut buf = vec![0u8; BUFSIZE];
    // the number of bytes currently packed into the chunk
    let mut cbytes = 0u32;
    // the number of records currently packed into the chunk
    let mut crec = 0u32;
    // the number of cells in the current chunk
    let mut cells_in_chunk = 0usize;
    // the offset of the first cell in this chunk
    let mut first_cell = 0usize;
    // if we had to expand the buffer already and should
    // forcibly push the current buffer onto the queue
    let mut force_push = false;
    // the number of bytes and records in the next chunk header
    let mut nbytes_chunk = 0u32;
    let mut nrec_chunk = 0u32;

    // we include the endpoint here because we will not actually
    // copy a chunk in the first iteration (since we have not yet
    // read the header, which comes at the end of the loop).
    for chunk_num in 0..=num_chunks {
        // in the first iteration we've not read a header yet
        // so we can't fill a chunk, otherwise we read the header
        // at the bottom of the previous iteration of this loop, and
        // we will fill in the buffer appropriately here.
        if chunk_num > 0 {
            // if the currenc cell (the cell whose header we read in the last iteration of
            // the loop) alone is too big for the buffer, than resize the buffer to be big enough
            if nbytes_chunk as usize > buf.len() {
                // if we had to resize the buffer to fit this cell, then make sure we push
                // immediately in the next round, unless we are skipping it's barcode
                force_push = true;
                let chunk_resize = nbytes_chunk as usize + cbytes as usize;
                buf.resize(chunk_resize, 0);
            }

            // copy the data for the current chunk into the buffer
            let boffset = cbytes as usize;
            buf.pwrite::<u32>(nbytes_chunk, boffset)?;
            buf.pwrite::<u32>(nrec_chunk, boffset + 4)?;
            br.read_exact(&mut buf[(boffset + 8)..(boffset + nbytes_chunk as usize)])
                .unwrap();
            // get the barcode for this chunk
            let (bc, _umi) =
                libradicl::Chunk::peek_record(&buf[boffset + 8..], &bc_type, &umi_type);
            if keep_set.contains(&bc) {
                cells_in_chunk += 1;
                cbytes += nbytes_chunk;
                crec += nrec_chunk;
            } else {
                // if we are skipping this cell, and it
                // triggered a force_push, then undo that
                force_push = false;
            }
        }

        // in the last iteration of the loop, we will have read num_chunks headers already
        // and we are just filling up the buffer with the last cell, and there will be no more
        // headers left to read, so skip this
        if chunk_num < num_chunks {
            let (nc, nr) = libradicl::Chunk::read_header(&mut br);
            nbytes_chunk = nc;
            nrec_chunk = nr;
        }

        // determine if we should dump the current buffer to the work queue
        if force_push  // if we were told to push this chunk
           || // or if adding the next cell to this chunk would exceed the buffer size
           ((cbytes + nbytes_chunk) as usize > buf.len() && cells_in_chunk > 0)
           || // of if this was the last chunk
           chunk_num == num_chunks
        {
            // launch off these cells on the queue
            let mut bclone = (first_cell, cells_in_chunk, cbytes, crec, buf.clone());
            // keep trying until we can push this payload
            while let Err(t) = q.push(bclone) {
                bclone = t;
                // no point trying to push if the queue is full
                while q.is_full() {}
            }
            pbar.inc(cells_in_chunk as u64);

            // offset of the first cell in the next chunk
            first_cell += cells_in_chunk;
            // reset the counters
            cells_in_chunk = 0;
            cbytes = 0;
            crec = 0;
            buf.resize(BUFSIZE, 0);
            force_push = false;
        }
    }
    Ok(())
}

// TODO: see if we'd rather pass an structure
// with these options
#[allow(clippy::too_many_arguments)]
pub fn quantify(
    input_dir: String,
    tg_map: String,
    output_dir: String,
    num_threads: u32,
    num_bootstraps: u32,
    init_uniform: bool,
    summary_stat: bool,
    dump_eq: bool,
    use_mtx: bool,
    resolution: ResolutionStrategy,
    small_thresh: usize,
    filter_list: Option<&str>,
    log: &slog::Logger,
) -> Result<(), Box<dyn std::error::Error>> {
    let parent = std::path::Path::new(&input_dir);
    let i_file = File::open(parent.join("map.collated.rad")).expect("run collate before quant");
    let mut br = BufReader::new(i_file);
    let hdr = libradicl::RadHeader::from_bytes(&mut br);

    // in the collated rad file, we have 1 cell per chunk.
    // we make this value `mut` since, if we have a non-empty
    // filter list, the number of cells will be dictated by
    // it's length.
    let mut num_cells = hdr.num_chunks;

    info!(
        log,
        "paired : {:?}, ref_count : {:?}, num_chunks : {:?}",
        hdr.is_paired != 0,
        hdr.ref_count,
        hdr.num_chunks
    );

    // now that we have the header, parse and convert the
    // tgmap.

    // first, build a hash of each transcript to it's index
    let rnhasher = ahash::RandomState::with_seeds(2u64, 7u64, 1u64, 8u64);
    let mut rname_to_id: HashMap<String, u32, ahash::RandomState> =
        HashMap::with_capacity_and_hasher(hdr.ref_count as usize, rnhasher);
    for (i, n) in hdr.ref_names.iter().enumerate() {
        rname_to_id.insert(n.clone(), i as u32);
    }
    //println!("{:?}", hdr.ref_names);

    // will hold the unique gene names in the order they are encountered
    let mut gene_names: Vec<String> = Vec::with_capacity((hdr.ref_count / 2) as usize);
    let gnhasher = ahash::RandomState::with_seeds(2u64, 7u64, 1u64, 8u64);
    let mut gene_name_to_id: HashMap<String, u32, ahash::RandomState> =
        HashMap::with_hasher(gnhasher);

    // now read in the transcript to gene map
    type TsvRec = (String, String);

    // map each transcript id to the corresponding gene id
    // the transcript name can be looked up from the id in the RAD header,
    // and the gene name can be looked up from the id in the gene_names
    // vector.
    let mut tid_to_gid = vec![u32::MAX; hdr.ref_count as usize];

    let t2g_file = std::fs::File::open(tg_map).expect("couldn't open file");
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .delimiter(b'\t')
        .from_reader(t2g_file);

    // now, map each transcript index to it's corresponding gene index
    let mut found = 0usize;
    for result in rdr.deserialize() {
        match result {
            Ok(record_in) => {
                let record: TsvRec = record_in;
                //let record: TSVRec = result?;
                // first, get the id for this gene
                let next_id = gene_name_to_id.len() as u32;
                let gene_id = *gene_name_to_id.entry(record.1.clone()).or_insert(next_id);
                // if we haven't added this gene name already, then
                // append it now to the list of gene names.
                if gene_id == next_id {
                    gene_names.push(record.1.clone());
                }
                // get the transcript id
                if let Some(transcript_id) = rname_to_id.get(&record.0) {
                    found += 1;
                    tid_to_gid[*transcript_id as usize] = gene_id;
                }
            }
            Err(e) => {
                crit!(
                    log,
                    "Encountered error [{}] when reading the transcript-to-gene map. Please make sure the transcript-to-gene mapping is a 2 column, tab separated file.",
                    e
                );
                return Err(Box::new(e));
            }
        }
    }

    assert_eq!(
        found, hdr.ref_count as usize,
        "The tg-map must contain a gene mapping for all transcripts in the header"
    );

    info!(
        log,
        "tg-map contained {} genes mapping to {} transcripts.",
        gene_names.len(),
        found
    );

    // read the map for the number of unmapped reads per corrected barcode
    let bc_unmapped_file =
        std::fs::File::open(parent.join("unmapped_bc_count_collated.bin")).unwrap();
    let bc_unmapped_map: Arc<HashMap<u64, u32>> =
        Arc::new(bincode::deserialize_from(&bc_unmapped_file).unwrap());

    // file-level
    let fl_tags = libradicl::TagSection::from_bytes(&mut br);
    info!(log, "read {:?} file-level tags", fl_tags.tags.len());
    // read-level
    let rl_tags = libradicl::TagSection::from_bytes(&mut br);
    info!(log, "read {:?} read-level tags", rl_tags.tags.len());
    // alignment-level
    let al_tags = libradicl::TagSection::from_bytes(&mut br);
    info!(log, "read {:?} alignemnt-level tags", al_tags.tags.len());

    let ft_vals = libradicl::FileTags::from_bytes(&mut br);
    info!(log, "File-level tag values {:?}", ft_vals);

    let bct = rl_tags.tags[0].typeid;
    let umit = rl_tags.tags[1].typeid;

    // if we have a filter list, extract it here
    let mut retained_bc: Option<HashSet<u64, ahash::RandomState>> = None;
    if let Some(fname) = filter_list {
        match read_filter_list(fname, ft_vals.bclen) {
            Ok(fset) => {
                // the number of cells we expect to
                // actually process
                num_cells = fset.len() as u64;
                retained_bc = Some(fset);
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    let mut _num_reads: usize = 0;

    let pbar = ProgressBar::new(num_cells);
    pbar.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg}",
            )
            .progress_chars("╢▌▌░╟"),
    );
    let ddelta = 500_u64.min(num_cells / 10);
    pbar.set_draw_delta(ddelta);

    // Trying this parallelization strategy to avoid
    // many temporary data structures.

    // We have a work queue that contains collated chunks
    // (i.e. data at the level of each cell).  One thread
    // populates the queue, and the remaining worker threads
    // pop a chunk, perform the quantification, and update the
    // output.  The updating of the output requires acquiring
    // two locks (1) to update the data in the matrix and
    // (2) to write to the barcode file.  We also have to
    // decrement an atomic coutner for the numebr of cells that
    // remain to be processed.

    // create a thread-safe queue based on the number of worker threads
    let n_workers = if num_threads > 1 {
        (num_threads - 1) as usize
    } else {
        1
    };
    let q = Arc::new(ArrayQueue::<MetaChunk>::new(4 * n_workers));

    // the number of cells left to process
    let cells_to_process = Arc::new(AtomicUsize::new(num_cells as usize));
    // each thread needs a *read-only* copy of this transcript <-> gene map
    let tid_to_gid_shared = std::sync::Arc::new(tid_to_gid);
    // the number of reference sequences
    let ref_count = hdr.ref_count as u32;
    // the types for the barcodes and umis
    let bc_type = libradicl::decode_int_type_tag(bct).expect("unsupported barcode type id.");
    let umi_type = libradicl::decode_int_type_tag(umit).expect("unsupported umi type id.");
    // the number of genes (different than the number of reference sequences, which are transcripts)
    let num_genes = gene_name_to_id.len();

    // create our output directory
    let output_path = std::path::Path::new(&output_dir);
    fs::create_dir_all(output_path)?;

    // create sub-directory for matrix
    let output_matrix_path = output_path.join("alevin");
    fs::create_dir_all(&output_matrix_path)?;

    // well need a protected handle to write out the barcode
    let bc_path = output_matrix_path.join("quants_mat_rows.txt");
    let bc_file = fs::File::create(bc_path)?;

    let mat_path = output_matrix_path.join("quants_mat.gz");
    let boot_helper = BootstrapHelper::new(output_path, num_bootstraps, summary_stat);
    let buffered = GzEncoder::new(fs::File::create(&mat_path)?, Compression::default());

    let ff_path = output_path.join("featureDump.txt");
    let mut ff_file = fs::File::create(ff_path)?;
    writeln!(
        ff_file,
        "CB\tCorrectedReads\tMappedReads\tDeduplicatedReads\tMappingRate\tDedupRate\tMeanByMax\tNumGenesExpressed\tNumGenesOverMean"
    )?;
    let alt_res_cells = Arc::new(Mutex::new(Vec::<u64>::new()));

    let tmcap = if use_mtx {
        (0.1f64 * num_genes as f64 * num_cells as f64).round() as usize
    } else {
        0usize
    };

    let trimat =
        sprs::TriMatI::<f32, u32>::with_capacity((num_cells as usize, num_genes as usize), tmcap);

    let bc_writer = Arc::new(Mutex::new(QuantOutputInfo {
        barcode_file: BufWriter::new(bc_file),
        eds_file: BufWriter::new(buffered),
        feature_file: BufWriter::new(ff_file),
        trimat,
        row_index: 0usize,
        bootstrap_helper: boot_helper,
    }));

    let mmrate = Arc::new(Mutex::new(vec![0f64; num_cells as usize]));

    let mut thread_handles: Vec<thread::JoinHandle<_>> = Vec::with_capacity(n_workers);

    // This is the hash table that will hold the global
    // (i.e. across all cells) gene-level equivalence
    // classes.  We have _not_ yet figured out an efficient
    // way to do this in a lock-free manner, so this
    // structure is protected by a lock for now.
    // This will only be used if the `dump_eq` paramater is true.
    let so = ahash::RandomState::with_seeds(2u64, 7u64, 1u64, 8u64);
    let eqid_map_lock = Arc::new(Mutex::new(EqcMap {
        global_eqc: HashMap::with_hasher(so),
        cell_level_count: Vec::new(),
        cell_offset: Vec::new(),
    }));

    // for each worker, spawn off a thread
    for _worker in 0..n_workers {
        // each thread will need to access the work queue
        let in_q = q.clone();
        // and the logger
        let log = log.clone();
        // the shared tid_to_gid map
        let tid_to_gid = tid_to_gid_shared.clone();
        // and the atomic counter of remaining work
        let cells_remaining = cells_to_process.clone();
        // they will need to know the bc and umi type
        let bc_type = bc_type;
        let umi_type = umi_type;
        // and the file writer
        let bcout = bc_writer.clone();
        // global gene-level eqc map
        let eqid_map_lockc = eqid_map_lock.clone();
        // and will need to know the barcode length
        let bclen = ft_vals.bclen;
        let alt_res_cells = alt_res_cells.clone();
        let unmapped_count = bc_unmapped_map.clone();
        let mmrate = mmrate.clone();

        // now, make the worker thread
        let handle = std::thread::spawn(move || {
            // these can be created once and cleared after processing
            // each cell.
            let mut unique_evidence = vec![false; num_genes];
            let mut no_ambiguity = vec![false; num_genes];
            let mut eq_map = EqMap::new(ref_count);
            let mut expressed_vec = Vec::<f32>::with_capacity(num_genes);
            let mut expressed_ind = Vec::<usize>::with_capacity(num_genes);
            let mut eds_bytes = Vec::<u8>::new();
            let mut bt_eds_bytes: Vec<u8> = Vec::new();
            let mut eds_mean_bytes: Vec<u8> = Vec::new();
            let mut eds_var_bytes: Vec<u8> = Vec::new();

            // the variable we will use to bind the *cell-specific* gene-level
            // equivalence class table.
            // Make gene-level eqclasses.
            // This is a map of gene ids to the count of
            // _de-duplicated_ reads observed for that set of genes.
            // For every gene set (label) of length 1, these are gene
            // unique reads.  Standard scRNA-seq counting results
            // can be obtained by simply discarding all equivalence
            // classes of size greater than 1, and probabilistic results
            // will attempt to resolve gene multi-mapping reads by
            // running an EM algorithm.
            let s = ahash::RandomState::with_seeds(2u64, 7u64, 1u64, 8u64);
            let mut gene_eqc: HashMap<Vec<u32>, u32, ahash::RandomState> = HashMap::with_hasher(s);

            let em_init_type = if init_uniform {
                EmInitType::Uniform
            } else {
                EmInitType::Informative
            };

            // pop MetaChunks from the work queue until everything is
            // processed
            while cells_remaining.load(Ordering::SeqCst) > 0 {
                if let Some((
                    first_cell_in_chunk,
                    cells_in_chunk,
                    _nbytes_total,
                    _nrec_total,
                    buf,
                )) = in_q.pop()
                {
                    // for every cell (chunk) within this meta-chunk
                    let mut byte_offset = 0usize;
                    for cn in 0..cells_in_chunk {
                        cells_remaining.fetch_sub(1, Ordering::SeqCst);
                        let cell_num = first_cell_in_chunk + cn;
                        // nbytes for the current cell
                        let nbytes = buf[byte_offset..].pread::<u32>(0).unwrap();
                        let nrec = buf[byte_offset..].pread::<u32>(4).unwrap();
                        let mut nbr =
                            BufReader::new(&buf[byte_offset..(byte_offset + nbytes as usize)]);
                        byte_offset += nbytes as usize;

                        let mut c = libradicl::Chunk::from_bytes(&mut nbr, &bc_type, &umi_type);
                        if c.reads.is_empty() {
                            warn!(log, "Discovered empty chunk; should not happen! cell_num = {}, nbytes = {}, nrec = {}", cell_num, nbytes, nrec);
                        }

                        // TODO: Clean up the expect() and merge with the check above
                        // the expect shouldn't happen, but the message is redundant with
                        // the above.  Plus, this would panic if it actually occurred.
                        let bc = c.reads.first().expect("chunk with no reads").bc;

                        // The structures we'll need to hold our output for this
                        // cell.
                        let mut counts: Vec<f32>;
                        let mut alt_resolution = false;

                        let mut bootstraps: Vec<Vec<f32>> = Vec::new();

                        let non_trivial = c.reads.len() >= small_thresh;
                        if non_trivial {
                            // TODO: some testing was done, but see if there
                            // is a better way to set this value.
                            let small_cell = c.reads.len() <= 250;

                            // TODO: Is there an easy / clean way to have similar
                            // optimized code paths for other resolution methods?

                            match resolution {
                                ResolutionStrategy::CellRangerLike
                                | ResolutionStrategy::CellRangerLikeEm => {
                                    if small_cell {
                                        pugutils::get_num_molecules_cell_ranger_like_small(
                                            &mut c,
                                            &tid_to_gid,
                                            num_genes,
                                            &mut gene_eqc,
                                            &log,
                                        );
                                    } else {
                                        eq_map.init_from_chunk(&mut c);
                                        pugutils::get_num_molecules_cell_ranger_like(
                                            &eq_map,
                                            &tid_to_gid,
                                            num_genes,
                                            &mut gene_eqc,
                                            &log,
                                        );
                                        eq_map.clear();
                                    }
                                    let only_unique =
                                        resolution == ResolutionStrategy::CellRangerLike;
                                    counts = em_optimize(
                                        &gene_eqc,
                                        &mut unique_evidence,
                                        &mut no_ambiguity,
                                        em_init_type,
                                        num_genes,
                                        only_unique,
                                        &log,
                                    );
                                }
                                ResolutionStrategy::Trivial => {
                                    eq_map.init_from_chunk(&mut c);
                                    let ct = pugutils::get_num_molecules_trivial_discard_all_ambig(
                                        &eq_map,
                                        &tid_to_gid,
                                        num_genes,
                                        &log,
                                    );
                                    counts = ct.0;
                                    mmrate.lock().unwrap()[cell_num] = ct.1;
                                    eq_map.clear();
                                }
                                ResolutionStrategy::Parsimony => {
                                    eq_map.init_from_chunk(&mut c);
                                    let g = extract_graph(&eq_map, &log);
                                    let pug_stats = pugutils::get_num_molecules(
                                        &g,
                                        &eq_map,
                                        &tid_to_gid,
                                        num_genes,
                                        &mut gene_eqc,
                                        &log,
                                    );
                                    alt_resolution = pug_stats.used_alternative_strategy; // alt_res;
                                    counts = em_optimize(
                                        &gene_eqc,
                                        &mut unique_evidence,
                                        &mut no_ambiguity,
                                        em_init_type,
                                        num_genes,
                                        true, // only unqique evidence
                                        &log,
                                    );
                                    eq_map.clear();
                                }
                                ResolutionStrategy::Full => {
                                    eq_map.init_from_chunk(&mut c);
                                    let g = extract_graph(&eq_map, &log);
                                    let pug_stats = pugutils::get_num_molecules(
                                        &g,
                                        &eq_map,
                                        &tid_to_gid,
                                        num_genes,
                                        &mut gene_eqc,
                                        &log,
                                    );
                                    alt_resolution = pug_stats.used_alternative_strategy; // alt_res;
                                    counts = em_optimize(
                                        &gene_eqc,
                                        &mut unique_evidence,
                                        &mut no_ambiguity,
                                        em_init_type,
                                        num_genes,
                                        false, // only unqique evidence
                                        &log,
                                    );
                                    eq_map.clear();
                                }
                            }

                            if num_bootstraps > 0 {
                                bootstraps = run_bootstrap(
                                    &gene_eqc,
                                    num_bootstraps,
                                    &counts,
                                    init_uniform,
                                    summary_stat,
                                    &log,
                                );
                            }

                            // clear our local variables
                            // eq_map.clear();

                            // fill requires >= 1.50.0
                            unique_evidence.fill(false);
                            no_ambiguity.fill(false);

                            // done clearing
                        } else {
                            // very small number of reads, avoid data structure
                            // overhead and resolve looking at the actual records
                            pugutils::get_num_molecules_cell_ranger_like_small(
                                &mut c,
                                &tid_to_gid,
                                num_genes,
                                &mut gene_eqc,
                                &log,
                            );
                            counts = vec![0f32; num_genes];
                            for (k, v) in gene_eqc.iter() {
                                if k.len() == 1 {
                                    counts[*k.first().unwrap() as usize] += *v as f32;
                                } else {
                                    match resolution {
                                        ResolutionStrategy::CellRangerLikeEm
                                        | ResolutionStrategy::Full => {
                                            let contrib = 1.0 / (k.len() as f32);
                                            for g in k.iter() {
                                                counts[*g as usize] += contrib;
                                            }
                                        }
                                        _ => {
                                            // otherwise discard gene multimappers
                                        }
                                    }
                                }
                            }

                            // if the user requested bootstraps
                            // NOTE: we check that the specified resolution method
                            // is conceptually compatible with bootstrapping before
                            // invoking `quant`, so we don't bother checking that
                            // here.
                            if num_bootstraps > 0 {
                                // TODO: should issue a warning here,
                                // bootstrapping doesn't make sense for
                                // unfiltered data.
                                if summary_stat {
                                    // sample mean = quant
                                    bootstraps.push(counts.clone());
                                    // sample var = 0
                                    bootstraps.push(vec![0f32; num_genes]);
                                } else {
                                    // no variation
                                    for _ in 0..num_bootstraps {
                                        bootstraps.push(counts.clone());
                                    }
                                }
                            } // if the user requested bootstraps
                        } // end of else branch for trivial size cells

                        if alt_resolution {
                            alt_res_cells.lock().unwrap().push(cell_num as u64);
                        }

                        //
                        // featuresStream << "\t" << numRawReads
                        //   << "\t" << numMappedReads
                        let mut max_umi = 0.0f32;
                        let mut sum_umi = 0.0f32;
                        let mut num_expr: u32 = 0;
                        expressed_vec.clear();
                        expressed_ind.clear();
                        for (gn, c) in counts.iter().enumerate() {
                            max_umi = if *c > max_umi { *c } else { max_umi };
                            sum_umi += *c;
                            if *c > 0.0 {
                                num_expr += 1;
                                expressed_vec.push(*c);
                                expressed_ind.push(gn);
                            }
                        }

                        let num_mapped = nrec;
                        let dedup_rate = sum_umi / num_mapped as f32;

                        let num_unmapped = match unmapped_count.get(&bc) {
                            Some(nu) => *nu,
                            None => 0u32,
                        };

                        let mapping_rate = num_mapped as f32 / (num_mapped + num_unmapped) as f32;

                        // mean of the "expressed" genes
                        let mean_expr = sum_umi / num_expr as f32;
                        // number of genes with expression > expressed mean
                        let num_genes_over_mean = expressed_vec.iter().fold(0u32, |acc, x| {
                            if x > &mean_expr {
                                acc + 1u32
                            } else {
                                acc
                            }
                        });
                        // expressed mean / max expression
                        let mean_by_max = mean_expr / max_umi;

                        let row_index: usize; // the index for this row (cell)
                        {
                            // writing the files
                            let bc_mer: BitKmer = (bc, bclen as u8);

                            if !use_mtx {
                                eds_bytes = sce::eds::as_bytes(&counts, num_genes)
                                    .expect("can't conver vector to eds");
                            }

                            // write bootstraps
                            if num_bootstraps > 0 {
                                // flatten the bootstraps
                                if summary_stat {
                                    eds_mean_bytes = sce::eds::as_bytes(&bootstraps[0], num_genes)
                                        .expect("can't convert vector to eds");
                                    eds_var_bytes = sce::eds::as_bytes(&bootstraps[1], num_genes)
                                        .expect("can't convert vector to eds");
                                } else {
                                    for i in 0..num_bootstraps {
                                        let bt_eds_bytes_slice =
                                            sce::eds::as_bytes(&bootstraps[i as usize], num_genes)
                                                .expect("can't convert vector to eds");
                                        bt_eds_bytes.append(&mut bt_eds_bytes_slice.clone());
                                    }
                                }
                            }

                            let writer_deref = bcout.lock();
                            let writer = &mut *writer_deref.unwrap();

                            // get the row index and then increment it
                            row_index = writer.row_index;
                            writer.row_index += 1;

                            // write to barcode file
                            let bc_bytes = &bitmer_to_bytes(bc_mer)[..];
                            writeln!(&mut writer.barcode_file, "{}", unsafe {
                                std::str::from_utf8_unchecked(&bc_bytes)
                            })
                            .expect("can't write to barcode file.");

                            // write to matrix file
                            if !use_mtx {
                                // write in eds format
                                writer
                                    .eds_file
                                    .write_all(&eds_bytes)
                                    .expect("can't write to matrix file.");
                            } else {
                                // fill out the triplet matrix in memory
                                for (ind, val) in expressed_ind.iter().zip(expressed_vec.iter()) {
                                    writer.trimat.add_triplet(row_index as usize, *ind, *val);
                                }
                            }
                            writeln!(
                                &mut writer.feature_file,
                                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                unsafe { std::str::from_utf8_unchecked(&bc_bytes) },
                                (num_mapped + num_unmapped),
                                num_mapped,
                                sum_umi,
                                mapping_rate,
                                dedup_rate,
                                mean_by_max,
                                num_expr,
                                num_genes_over_mean
                            )
                            .expect("can't write to feature file");

                            if num_bootstraps > 0 {
                                if summary_stat {
                                    if let Some((meanf, varf)) =
                                        &mut writer.bootstrap_helper.mean_var_files
                                    {
                                        meanf
                                            .write_all(&eds_mean_bytes)
                                            .expect("can't write to bootstrap mean file.");
                                        varf.write_all(&eds_var_bytes)
                                            .expect("can't write to bootstrap var file.");
                                    }
                                } else if let Some(bsfile) = &mut writer.bootstrap_helper.bsfile {
                                    bsfile
                                        .write_all(&bt_eds_bytes)
                                        .expect("can't write to bootstrap file");
                                }
                            } // done bootstrap writing
                        }

                        // if we are dumping the equivalence class output, fill in
                        // the in-memory representation here.
                        if dump_eq {
                            let eqmap_deref = eqid_map_lockc.lock();
                            let geqmap = &mut *eqmap_deref.unwrap();
                            // the next available global id for a gene-level
                            // equivalence class
                            let mut next_id = geqmap.global_eqc.len() as u64;
                            for (labels, count) in gene_eqc.iter() {
                                let mut found = true;
                                match geqmap.global_eqc.get(&labels.to_vec()) {
                                    Some(eqid) => {
                                        geqmap.cell_level_count.push((*eqid, *count));
                                    }
                                    None => {
                                        found = false;
                                        geqmap.cell_level_count.push((next_id, *count));
                                    }
                                }
                                if !found {
                                    geqmap.global_eqc.insert(labels.to_vec().clone(), next_id);
                                    next_id += 1;
                                }
                            }
                            //let bc_mer: BitKmer = (bc, bclen as u8);
                            geqmap.cell_offset.push((row_index, gene_eqc.len()));
                        }
                        // clear the gene eqc map
                        gene_eqc.clear();
                    } // for all cells in this meta chunk
                } // while we can get work
            } // while cells remain
        });

        thread_handles.push(handle);
    }

    // push the work onto the queue for the worker threads
    // we spawned above.
    if let Some(ret_bc) = retained_bc {
        // we have a retained set
        fill_work_queue_filtered(ret_bc, &rl_tags, q, br, hdr.num_chunks as usize, &pbar)?;
    } else {
        // we're quantifying everything
        fill_work_queue(q, br, hdr.num_chunks as usize, &pbar)?;
    }

    let gn_path = output_matrix_path.join("quants_mat_cols.txt");
    let gn_file = File::create(gn_path).expect("couldn't create gene name file.");
    let mut gn_writer = BufWriter::new(gn_file);
    for g in gene_names {
        gn_writer.write_all(format!("{}\n", g).as_bytes())?;
    }

    for h in thread_handles {
        match h.join() {
            Ok(_) => {}
            Err(_e) => {
                info!(log, "thread panicked");
            }
        }
    }

    // write to matrix market if we are using it
    if use_mtx {
        let writer_deref = bc_writer.lock();
        let writer = &mut *writer_deref.unwrap();
        writer.eds_file.flush().unwrap();
        // now remove it
        fs::remove_file(&mat_path)?;
        let mtx_path = output_matrix_path.join("quants_mat.mtx");
        sprs::io::write_matrix_market(&mtx_path, &writer.trimat)?;
    }

    let pb_msg = format!("finished quantifying {} cells.", num_cells);
    pbar.finish_with_message(&pb_msg);

    if dump_eq {
        write_eqc_counts(&eqid_map_lock, num_genes, &output_path, &log);
    }

    let meta_info = json!({
        "resolution_strategy" : resolution.to_string(),
        "num_quantified_cells" : num_cells,
        "num_genes" : num_genes,
        "dump_eq" : dump_eq,
        "alt_resolved_cell_numbers" : *alt_res_cells.lock().unwrap()
    });

    let mut meta_info_file = File::create(output_path.join("meta_info.json"))
        .expect("couldn't create meta_info.json file.");
    let aux_info_str = serde_json::to_string_pretty(&meta_info).expect("could not format json.");
    meta_info_file
        .write_all(aux_info_str.as_bytes())
        .expect("cannot write to meta_info.json file");

    // k3yavi: Todo delete after api stability
    // creating a dummy cmd_info.json for R compatibility
    let cmd_info = json!({
         "salmon_version": "1.3.0",
         "auxDir": "aux_info"
    });
    let mut cmd_info_file = File::create(output_path.join("cmd_info.json"))
        .expect("couldn't create cmd_info.json file.");
    let cmd_info_str =
        serde_json::to_string_pretty(&cmd_info).expect("could not format cmd_info json.");
    cmd_info_file
        .write_all(cmd_info_str.as_bytes())
        .expect("cannot write to cmd_info.json file");

    Ok(())
}

// TODO: see if we'd rather pass an structure
// with these options
#[allow(clippy::too_many_arguments)]
pub fn velo_quantify(
    _input_dir: String,
    _tg_map: String,
    _output_dir: String,
    _num_threads: u32,
    _num_bootstraps: u32,
    _init_uniform: bool,
    _summary_stat: bool,
    _dump_eq: bool,
    _use_mtx: bool,
    _resolution: ResolutionStrategy,
    _small_thresh: usize,
    _filter_list: Option<&str>,
    _log: &slog::Logger,
) -> Result<(), Box<dyn std::error::Error>> {
    unimplemented!("not implemented on this branch yet");
    //Ok(())
}
