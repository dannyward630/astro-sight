use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor};

use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

use super::{
    MemberCandidate, MemberStatus, SetAccum, collect_source_files, is_class_member_kind,
    status_from_counts,
};

/// TS/JS class member の owner-aware liveness インデックス。
///
/// key = (file_rel, owner_class, bare_member)。dead-code 検出側で
/// `JsTsMemberLiveness::status_for` を呼び出して `MemberStatus` を取得する。
#[derive(Debug, Default)]
pub(crate) struct JsTsMemberLiveness {
    statuses: HashMap<(String, String, String), MemberStatus>,
}

impl JsTsMemberLiveness {
    /// 候補シンボル群から duplicate な TS/JS class member の owner-aware liveness を解析する。
    ///
    /// `candidates` は `(name, kind, file_rel, lang)` のタプル列。`name` は qualname 形式
    /// (`Container.member`)。`canonical_dir` は走査対象のルート (canonicalize 済み)。
    /// `extra_files` は hidden ディレクトリ配下などの walk 対象外でも候補になった
    /// diff 由来ファイル (count 経路と走査集合を一致させる)。`is_test_path` は
    /// test ディレクトリ判定。
    pub(crate) fn build<F>(
        candidates: &[(String, String, String, LangId)],
        canonical_dir: &Path,
        extra_files: &[std::path::PathBuf],
        is_test_path: F,
    ) -> Self
    where
        F: Fn(&Path) -> bool + Sync,
    {
        let mut statuses: HashMap<(String, String, String), MemberStatus> = HashMap::new();

        // Step 1: TS/JS class member candidate を集める。
        let mut js_ts_members: Vec<MemberCandidate> = Vec::new();
        for (name, kind, file, lang) in candidates {
            if !is_js_ts_lang(*lang) {
                continue;
            }
            if !is_class_member_kind(kind) {
                continue;
            }
            let Some((owner, bare)) = name.rsplit_once('.') else {
                continue;
            };
            // 多段 qualname (例: namespace.Class.member) は class.member とみなさない。
            if owner.contains('.') {
                continue;
            }
            js_ts_members.push(MemberCandidate {
                owner: owner.to_string(),
                bare: bare.to_string(),
                file: file.clone(),
            });
        }
        if js_ts_members.is_empty() {
            return Self { statuses };
        }

        // Step 2: bare name 単位で duplicate set を構築。
        let mut bare_to_members: HashMap<&str, Vec<&MemberCandidate>> = HashMap::new();
        for m in &js_ts_members {
            bare_to_members.entry(m.bare.as_str()).or_default().push(m);
        }

        // Step 2.5: duplicate owner を持つ set がなければ全ファイル収集を skip して早期 return。
        let has_duplicate_set = bare_to_members.values().any(|v| {
            let owners: HashSet<&str> = v.iter().map(|m| m.owner.as_str()).collect();
            owners.len() >= 2
        });
        if !has_duplicate_set {
            return Self { statuses };
        }

        // Step 3: TS/JS ファイル一覧を収集 (一度だけ)。hidden 配下の diff 候補も合流させる。
        let Some(all_files) = collect_source_files(canonical_dir, extra_files) else {
            return Self { statuses };
        };
        // 拡張子なしの `bin/cli` (`#!/usr/bin/env node`) も参照件数の経路と同じく含める。
        let ts_js_files: Vec<(std::path::PathBuf, LangId)> = all_files
            .into_iter()
            .filter_map(|p| {
                let lang = crate::engine::refs::detect_source_lang(&p)?;
                if is_js_ts_lang(lang) {
                    Some((p, lang))
                } else {
                    None
                }
            })
            .collect();

        // Step 4: duplicate set を先に確定し、ファイル毎に 1 回だけ read + parse して
        // 全 set をまとめて解析する (ループ反転)。旧実装は set 毎に全 TS/JS ファイルを
        // read + parse + クエリ実行しており、set 数 × ファイル数の再パースが dead-code
        // 全体の支配的コストだった。ファイル間の集計は可換 (counts の加算 / ambiguous の
        // OR) なので rayon 並列でも結果は入力順に依存しない。
        let sets: Vec<(&str, Vec<&str>)> = bare_to_members
            .iter()
            .filter_map(|(bare, members)| {
                // 同 owner で同 member 名を 2 回 export するケースは通常ないため、
                // owner 単位の uniq に正規化する。
                let owners: HashSet<&str> = members.iter().map(|m| m.owner.as_str()).collect();
                if owners.len() < 2 {
                    // duplicate owner でない場合は対象外 (`name_counts > 1` も発生しない
                    // 想定だが、同じ owner が複数ファイルに同名 export を持つ稀ケースは
                    // 旧スキップに任せる)。
                    return None;
                }
                let mut owners: Vec<&str> = owners.into_iter().collect();
                owners.sort_unstable();
                Some((*bare, owners))
            })
            .collect();

        let accums = analyze_sets_over_files(&sets, &ts_js_files, &is_test_path);

        for ((bare, _owners), accum) in sets.iter().zip(accums) {
            let members = &bare_to_members[bare];
            if accum.ambiguous {
                for m in members {
                    statuses.insert(
                        (m.file.clone(), m.owner.clone(), (*bare).to_string()),
                        MemberStatus::Ambiguous,
                    );
                }
            } else {
                for m in members {
                    let (prod, tst) = accum
                        .counts
                        .get(m.owner.as_str())
                        .copied()
                        .unwrap_or((0, 0));
                    let status = status_from_counts(prod, tst);
                    statuses.insert(
                        (m.file.clone(), m.owner.clone(), (*bare).to_string()),
                        status,
                    );
                }
            }
        }

        Self { statuses }
    }

    /// 指定 (file, owner, bare) の MemberStatus を返す。エントリがなければ None。
    pub(crate) fn status_for(&self, owner: &str, bare: &str, file: &str) -> Option<MemberStatus> {
        self.statuses
            .get(&(file.to_string(), owner.to_string(), bare.to_string()))
            .copied()
    }
}

/// 全 duplicate set を、ファイル毎 1 回の read + parse で並列に集計する。
///
/// 旧 `analyze_duplicate_set` は set 毎に全ファイルを read + parse していた
/// (set 数 × ファイル数の再パース)。本関数はループを反転し、1 ファイルを 1 回だけ
/// parse して出現する全 set を解析する。ambiguous の OR と counts の加算は可換なので、
/// rayon の fold/reduce 順に依らず結果は決定的。旧実装の「ambiguous 確定後は残り
/// ファイルを見ない」短絡は、worker 局所の ambiguous フラグでファイル単位に再現する
/// (worker を跨ぐ短絡はしないが、結果は同一で走査量だけ最大 worker 数分増える)。
fn analyze_sets_over_files<F>(
    sets: &[(&str, Vec<&str>)],
    files: &[(std::path::PathBuf, LangId)],
    is_test_path: &F,
) -> Vec<SetAccum>
where
    F: Fn(&Path) -> bool + Sync,
{
    use rayon::prelude::*;

    if sets.is_empty() {
        return Vec::new();
    }

    files
        .par_iter()
        .fold(
            || vec![SetAccum::default(); sets.len()],
            |mut acc, (file_path, lang)| {
                analyze_one_file_into(sets, file_path, *lang, is_test_path, &mut acc);
                acc
            },
        )
        .reduce(
            || vec![SetAccum::default(); sets.len()],
            |mut merged, local| {
                for (m, l) in merged.iter_mut().zip(local) {
                    m.merge(l);
                }
                merged
            },
        )
}

/// 1 ファイルを解析して該当する全 set の accum を更新する。parse とファイル内の事実の
/// 収集は最初に必要になった set の時点で 1 回だけ行い、以降の set は同じ事実を使い回す。
fn analyze_one_file_into<F>(
    sets: &[(&str, Vec<&str>)],
    file_path: &Path,
    lang: LangId,
    is_test_path: &F,
    acc: &mut [SetAccum],
) where
    F: Fn(&Path) -> bool,
{
    let Some(path_str) = file_path.to_str() else {
        return;
    };
    let Ok(source) = parser::read_file(camino::Utf8Path::new(path_str)) else {
        return;
    };

    // parse は該当 set が 1 つでもあるときに 1 回だけ。失敗も 1 回で確定させ、
    // 以降の set は旧実装の per-set continue と同じくこのファイルをスキップする。
    let parsed: OnceCell<Option<tree_sitter::Tree>> = OnceCell::new();
    // owner 集合に依存しない事実 (クラス名・束縛・member access) は木を 1 回だけ辿って集め、
    // set ごとには owner 集合での絞り込みだけを行う。旧実装は set ごとに木全体の走査と
    // 3 本のクエリをやり直しており、duplicate set の多いリポジトリで dead-code の大半を占めた。
    let facts: OnceCell<FileFacts<'_>> = OnceCell::new();
    let mut is_test: Option<bool> = None;

    for ((bare, owners), set_acc) in sets.iter().zip(acc.iter_mut()) {
        // この worker で既に ambiguous 確定した set は解析しない (sticky のため結果不変)。
        if set_acc.ambiguous {
            continue;
        }
        // バイトレベルで bare member が含まれないファイルは parse skip (高速化)。
        if memchr::memmem::find(source.as_bytes(), bare.as_bytes()).is_none() {
            continue;
        }

        // ambiguous source の早期検出 (string literal / computed access)。
        if contains_ambiguous_member_token(source.as_bytes(), bare) {
            set_acc.ambiguous = true;
            continue;
        }

        let Some(tree) = parsed.get_or_init(|| parser::parse_source(&source, lang).ok()) else {
            return;
        };
        let facts =
            facts.get_or_init(|| FileFacts::collect(tree.root_node(), source.as_bytes(), lang));
        let analysis = facts.analyze(owners, bare);

        // receiver を owner へ静的に辿れない access (factory / DI / 関数戻り値・
        // 引数経由のインスタンス等) が 1 件でもあれば、票の行き先を import 有無で
        // 推測せず duplicate set 全体を Ambiguous に倒す (旧スキップへフォールバック)。
        if analysis.has_unresolved_access {
            set_acc.ambiguous = true;
            continue;
        }
        if analysis.resolved_counts.is_empty() {
            continue;
        }

        let is_test = *is_test.get_or_insert_with(|| is_test_path(file_path));
        for (owner, count) in analysis.resolved_counts {
            let entry = set_acc.counts.entry(owner).or_insert((0, 0));
            if is_test {
                entry.1 = entry.1.saturating_add(count);
            } else {
                entry.0 = entry.0.saturating_add(count);
            }
        }
    }
}

/// ファイル内 1 つの bare member について receiver 解決済み access を集計する。
struct FileAnalysis {
    /// receiver が owner へ静的に辿れた access の owner 別出現回数 (production/test 区別前)。
    resolved_counts: HashMap<String, usize>,
    /// receiver を owner へ静的に辿れない access があったか。
    has_unresolved_access: bool,
}

/// ファイル内で owner 集合に依存しない事実。duplicate set ごとに木を辿り直さないよう、
/// ファイルごとに 1 回だけ集める。
///
/// owner 集合に依存するのは名前の解決 (クラス名として使えるローカル名の絞り込み、束縛の
/// 型の解決、`this` の enclosing class の照合) だけなので、`analyze` が set ごとにそれだけを
/// 行う。`analyze` は旧実装 (set ごとに木全体を走査し、owner 集合で絞りながら表を作っていた)
/// と同じ結果を返す。
struct FileFacts<'a> {
    source: &'a [u8],
    /// クラス名として使えるローカル名と、それが指す owner 名の候補 (ローカル名, owner 名)。
    /// import → 同ファイルの class 定義の順 (旧実装の insert 順) に並べる。owner 名が set の
    /// owner に含まれるものだけを順に入れ、後勝ちにすると旧実装の表と同じになる。
    class_names: Vec<(&'a str, &'a str)>,
    /// ローカル変数 / パラメータの束縛名 → 束縛の内容。
    bindings: HashMap<&'a str, BindingFact<'a>>,
    /// `obj.member` / `obj?.member` の member 名 → その member_expression (出現順)。
    member_accesses: HashMap<&'a str, Vec<Node<'a>>>,
    /// 分割代入パターンのキー位置に現れた名前。
    destructured_keys: HashSet<&'a str>,
}

/// 名前ごとの束縛。同じ名前が 2 回以上束縛されたら、スコープを解決せずに辿れないもの
/// (`Multiple`) として扱う (shadow / 別スコープの一意解決を諦める)。
enum BindingFact<'a> {
    Single(BindingSpec<'a>),
    Multiple,
}

/// 束縛 1 件の内容。owner への解決は set の owner 集合で行う。
#[derive(Clone, Copy)]
enum BindingSpec<'a> {
    /// 単純 identifier の束縛。型注釈 (`a: Alpha` / `xs: Alpha[]`) を最優先し、解決できなければ
    /// `= new Alpha()` の初期化子で owner を辿る (パラメータは型注釈だけ)。
    Typed {
        annotation: Option<TypeRef<'a>>,
        constructor: Option<&'a str>,
    },
    /// destructuring・loop 変数・catch・関数名などで、型を辿れない束縛。
    Unresolvable,
}

/// 型注釈が指す型名。
#[derive(Clone, Copy)]
enum TypeRef<'a> {
    /// `Alpha`
    Class(&'a str),
    /// `Alpha[]` (要素が owner。`xs[i].member` で使う)
    ArrayOf(&'a str),
}

/// set の owner 集合で解決したローカル変数 / パラメータの束縛。
enum VarBinding<'a> {
    /// `const a = new Alpha()` / `a: Alpha` のように単一 owner へ辿れる。
    Owner(&'a str),
    /// `const xs: Alpha[] = ...` のように owner の配列型。`xs[i].member` で owner に辿れる。
    ElementOwner(&'a str),
    /// factory 戻り値・非 owner 型・同名バインディング複数などで辿れない。
    Unresolvable,
}

impl<'a> FileFacts<'a> {
    fn collect(root: Node<'a>, source: &'a [u8], lang: LangId) -> Self {
        let mut facts = FileFacts {
            source,
            class_names: Vec::new(),
            bindings: HashMap::new(),
            member_accesses: HashMap::new(),
            destructured_keys: HashSet::new(),
        };
        let has_grammar = ts_language_for(lang).is_some();

        // 1. クラス名として使えるローカル名。
        //    - import (default / named / alias): `import { Alpha as A }` → A → Alpha
        //    - 同ファイル内 class 定義: class Alpha {} → Alpha → Alpha
        //    `import * as ns` 経由のアクセス (`ns.Alpha.fmt()`) は receiver が
        //    member_expression になり通常の分類で不解決に落ちるため、namespace import の
        //    存在だけでファイル全体を Ambiguous にはしない (対象 member への access が
        //    無いファイルまで巻き込むのを避ける)。
        if has_grammar
            && let Ok(query) = crate::engine::query_cache::cached_query(lang, IMPORT_QUERY)
        {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, root, source);
            while let Some(m) = matches.next() {
                let mut import_name: Option<&'a str> = None;
                let mut import_alias: Option<&'a str> = None;
                for cap in m.captures() {
                    let Ok(text) = cap.node.utf8_text(source) else {
                        continue;
                    };
                    match query.capture_names()[cap.index as usize] {
                        "default_name" => facts.class_names.push((text, text)),
                        "import_name" => import_name = Some(text),
                        "import_alias" => import_alias = Some(text),
                        _ => {}
                    }
                }
                if let Some(name) = import_name {
                    facts.class_names.push((import_alias.unwrap_or(name), name));
                }
            }
        }
        collect_local_class_definitions(root, source, &mut facts.class_names);

        // 2. ローカル変数 / パラメータの束縛。
        collect_binding_facts(root, source, &mut facts.bindings);

        // 3. `obj.member` / `obj?.member`。receiver の分類は set ごとに `analyze` で行う。
        if has_grammar
            && let Ok(query) = crate::engine::query_cache::cached_query(lang, MEMBER_ACCESS_QUERY)
        {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, root, source);
            while let Some(m) = matches.next() {
                for cap in m.captures() {
                    let node = cap.node;
                    let Ok(text) = node.utf8_text(source) else {
                        continue;
                    };
                    let Some(member_expr) = node.parent() else {
                        continue;
                    };
                    facts
                        .member_accesses
                        .entry(text)
                        .or_default()
                        .push(member_expr);
                }
            }
        }

        // 4. 分割代入パターンのキー位置の名前 (`analyze` の説明を参照)。
        if has_grammar
            && let Ok(query) =
                crate::engine::query_cache::cached_query(lang, DESTRUCTURED_KEY_QUERY)
        {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, root, source);
            while let Some(m) = matches.next() {
                for cap in m.captures() {
                    if let Ok(text) = cap.node.utf8_text(source) {
                        facts.destructured_keys.insert(text);
                    }
                }
            }
        }

        facts
    }

    /// owner 集合 `target_owners` の duplicate set について、member `target_member` への
    /// access の receiver を分類する。
    fn analyze(&self, target_owners: &[&str], target_member: &str) -> FileAnalysis {
        let mut analysis = FileAnalysis {
            resolved_counts: HashMap::new(),
            has_unresolved_access: false,
        };
        let owners_set: HashSet<&str> = target_owners.iter().copied().collect();
        let local_to_owner = self.local_to_owner(&owners_set);

        // `obj.member` / `obj?.member` の receiver を分類して票を入れる。
        for &member_expr in self
            .member_accesses
            .get(target_member)
            .into_iter()
            .flatten()
        {
            match self.resolve_member_receiver(member_expr, &local_to_owner, &owners_set) {
                Some(owner) => {
                    *analysis
                        .resolved_counts
                        .entry(owner.to_string())
                        .or_default() += 1;
                }
                None => {
                    analysis.has_unresolved_access = true;
                    return analysis;
                }
            }
        }

        // 分割代入でメンバーを取り出す access (`const { parse } = Alpha` /
        // `function f({ parse })` / `({ parse } = x)`) は member_expression にならないため
        // 上の分類に掛からず、分割代入でしか使われない同名メンバーが両方 dead に出ていた。
        // 右辺の receiver を辿れたとしても取り出した値の行き先は追わないので、帰属を推測せず
        // パターンのキーに同名が現れた時点で不解決 (duplicate set 全体を Ambiguous) に倒す。
        if self.destructured_keys.contains(target_member) {
            analysis.has_unresolved_access = true;
        }
        analysis
    }

    /// クラス名として使えるローカル名 → owner の表 (owner が set に含まれるものだけ)。
    fn local_to_owner(&self, owners_set: &HashSet<&str>) -> HashMap<&'a str, &'a str> {
        let mut local_to_owner = HashMap::new();
        for &(local, owner) in &self.class_names {
            if owners_set.contains(owner) {
                local_to_owner.insert(local, owner);
            }
        }
        local_to_owner
    }

    /// 束縛 `name` を set の owner 集合で解決する。束縛が無ければ `None`。
    fn binding(
        &self,
        name: &str,
        local_to_owner: &HashMap<&'a str, &'a str>,
    ) -> Option<VarBinding<'a>> {
        let binding = match self.bindings.get(name)? {
            BindingFact::Multiple | BindingFact::Single(BindingSpec::Unresolvable) => {
                VarBinding::Unresolvable
            }
            BindingFact::Single(BindingSpec::Typed {
                annotation,
                constructor,
            }) => annotation
                .and_then(|ty| match ty {
                    TypeRef::Class(class) => {
                        resolve_class_name(class, local_to_owner).map(VarBinding::Owner)
                    }
                    TypeRef::ArrayOf(class) => {
                        resolve_class_name(class, local_to_owner).map(VarBinding::ElementOwner)
                    }
                })
                .or_else(|| {
                    constructor
                        .and_then(|class| resolve_class_name(class, local_to_owner))
                        .map(VarBinding::Owner)
                })
                .unwrap_or(VarBinding::Unresolvable),
        };
        Some(binding)
    }

    /// `member_expression` の receiver (object 側) を owner へ静的に解決する。
    /// 解決対象: owner 名の static access / `new Owner().m()` / owner 束縛済み変数 /
    /// class 内 `this.m()` (enclosing class が owner の場合)。それ以外は `None` (不解決)。
    fn resolve_member_receiver(
        &self,
        member_expr: Node<'a>,
        local_to_owner: &HashMap<&'a str, &'a str>,
        owners_set: &HashSet<&str>,
    ) -> Option<&'a str> {
        let source = self.source;
        let mut object = member_expr.child_by_field_name("object")?;
        // 冗長括弧 (`(new Owner()).member` / `(alpha).member`) は剥がして実 receiver で
        // 判定する (剥がさないと Ambiguous に落ちて duplicate set の検出力が下がる)。
        while object.kind() == "parenthesized_expression" {
            object = object.named_child(0)?;
        }
        match object.kind() {
            "identifier" => {
                let text = object.utf8_text(source).ok()?;
                // 同名のローカル binding があれば shadow とみなし変数として判定する
                // (`const Alpha = getBeta(); Alpha.fmt()` を class static access と
                // 誤認しない)。binding が無い場合のみ import / 同一ファイル class 名。
                if let Some(binding) = self.binding(text, local_to_owner) {
                    return match binding {
                        VarBinding::Owner(owner) => Some(owner),
                        _ => None,
                    };
                }
                resolve_class_name(text, local_to_owner)
            }
            "new_expression" => new_expression_class(object, source)
                .and_then(|class| resolve_class_name(class, local_to_owner)),
            // `xs[i].member`: xs が owner 配列型 (`Alpha[]`) に束縛されていれば要素は owner。
            "subscript_expression" => {
                let array = object.child_by_field_name("object")?;
                if array.kind() != "identifier" {
                    return None;
                }
                let text = array.utf8_text(source).ok()?;
                match self.binding(text, local_to_owner) {
                    Some(VarBinding::ElementOwner(owner)) => Some(owner),
                    _ => None,
                }
            }
            "this" => {
                let class_name = enclosing_class_name(member_expr, source)?;
                if owners_set.contains(class_name) {
                    Some(class_name)
                } else {
                    // enclosing class が owner でない `this.m()` は継承経由で owner へ
                    // 到達し得るため、推測せず不解決に倒す。
                    None
                }
            }
            _ => None,
        }
    }
}

/// 同ファイル内の class 定義名を、クラス名として使えるローカル名の候補に加える。
///
/// ファイル全体を辿るため、子 1 段ごとに再帰すると深い配列・式を持つファイル 1 つで
/// dead-code ごと落ちる。named_children の再帰と同じ順序で反復して辿る。
fn collect_local_class_definitions<'a>(
    root: Node<'a>,
    source: &'a [u8],
    class_names: &mut Vec<(&'a str, &'a str)>,
) {
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        if matches!(node.kind(), "class_declaration" | "class")
            && let Some(name) = node.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
        {
            class_names.push((text, text));
        }
        ControlFlow::Continue(Walk::Descend)
    });
}

/// 束縛を 1 件登録する。同名の束縛が既にあれば `Multiple` に落とす
/// (shadow / 別スコープの一意解決を諦める)。
fn register_binding<'a>(
    bindings: &mut HashMap<&'a str, BindingFact<'a>>,
    name: &'a str,
    spec: BindingSpec<'a>,
) {
    use std::collections::hash_map::Entry;
    match bindings.entry(name) {
        Entry::Vacant(e) => {
            e.insert(BindingFact::Single(spec));
        }
        Entry::Occupied(mut e) => {
            e.insert(BindingFact::Multiple);
        }
    }
}

/// binding パターン (identifier / destructuring / rest / default) 配下の束縛名を
/// すべて `Unresolvable` として登録する。中身の型は静的に辿れないため owner 推論は
/// しない。収集漏れは shadow 見逃し → import owner への誤帰属 (fail-open) になる
/// ため、未知の構造は全 named children を辿って leaf の identifier を漏らさない。
///
/// 束縛位置の規則 (`{ key: alias }` は value 側、default 付きは left 側だけ) はシンボル抽出や
/// shadow 判定と同じなので `visit_pattern_bindings` を使う。深い入れ子のパターンでも
/// スタックを使い切らない。
fn register_pattern_bindings<'a>(
    pattern: Node<'a>,
    source: &'a [u8],
    bindings: &mut HashMap<&'a str, BindingFact<'a>>,
) {
    let _ = crate::engine::js_binding_pattern::visit_pattern_bindings(pattern, &mut |binding| {
        if let Ok(name) = binding.utf8_text(source) {
            register_binding(bindings, name, BindingSpec::Unresolvable);
        }
        ControlFlow::Continue(())
    });
}

/// ローカル変数 / パラメータの束縛を集める。
///
/// owner への静的解決を試みるのは「単純 identifier + 型注釈 / `new` 初期化子」の
/// 経路のみ。それ以外の binding 導入構文 (destructuring / for-of・for-in の宣言付き
/// loop 変数 / catch / bare arrow・JS パラメータ / 関数宣言・named 式の名前) は
/// `Unresolvable` として登録する — 収集しないと receiver 解決が import 由来の
/// class static access へフォールバックし、shadow された名前の票を owner へ誤計上
/// する (dead-code の fail-open)。
///
/// ファイル全体を辿るので、named_children の再帰と同じ順序で反復して辿る
/// (深い配列・式を持つファイル 1 つで dead-code ごと落ちていた)。
fn collect_binding_facts<'a>(
    root: Node<'a>,
    source: &'a [u8],
    bindings: &mut HashMap<&'a str, BindingFact<'a>>,
) {
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        collect_binding_facts_at(node, source, bindings);
        ControlFlow::Continue(Walk::Descend)
    });
}

/// `collect_binding_facts` の 1 ノード分の処理 (子へは降りない)。
fn collect_binding_facts_at<'a>(
    node: Node<'a>,
    source: &'a [u8],
    bindings: &mut HashMap<&'a str, BindingFact<'a>>,
) {
    match node.kind() {
        "variable_declarator" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                if name_node.kind() == "identifier" {
                    if let Ok(name) = name_node.utf8_text(source) {
                        // 型注釈が最優先 (`const a: Alpha = ...`)、次に `= new Alpha()`。
                        let spec = BindingSpec::Typed {
                            annotation: node
                                .child_by_field_name("type")
                                .and_then(|t| type_annotation_ref(t, source)),
                            constructor: node
                                .child_by_field_name("value")
                                .and_then(|v| new_expression_class(v, source)),
                        };
                        register_binding(bindings, name, spec);
                    }
                } else {
                    // destructuring (`const { a } = ...`) は要素の型を辿れない。
                    register_pattern_bindings(name_node, source, bindings);
                }
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(pat) = node.child_by_field_name("pattern") {
                if pat.kind() == "identifier" {
                    if let Ok(name) = pat.utf8_text(source) {
                        let spec = BindingSpec::Typed {
                            annotation: node
                                .child_by_field_name("type")
                                .and_then(|t| type_annotation_ref(t, source)),
                            constructor: None,
                        };
                        register_binding(bindings, name, spec);
                    }
                } else {
                    register_pattern_bindings(pat, source, bindings);
                }
            }
        }
        // JS の関数パラメータは formal_parameters 直下に bare pattern で並ぶ
        // (TS は required/optional_parameter に包まれ上の arm が処理する)。
        "formal_parameters" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !matches!(child.kind(), "required_parameter" | "optional_parameter") {
                    register_pattern_bindings(child, source, bindings);
                }
            }
        }
        // arrow の単独パラメータ (`x => ...`) は formal_parameters に包まれない。
        "arrow_function" => {
            if let Some(p) = node.child_by_field_name("parameter") {
                register_pattern_bindings(p, source, bindings);
            }
        }
        // for-of / for-in の宣言付き loop 変数 (`for (const x of xs)`)。left は
        // lexical_declaration に包まれない bare pattern のため専用処理が要る。
        // kind (var/let/const) の無い `for (x of xs)` は既存変数への代入であって
        // 新規 binding ではないため対象外。
        "for_in_statement" => {
            if node.child_by_field_name("kind").is_some()
                && let Some(left) = node.child_by_field_name("left")
            {
                register_pattern_bindings(left, source, bindings);
            }
        }
        // catch (e) の e。
        "catch_clause" => {
            if let Some(p) = node.child_by_field_name("parameter") {
                register_pattern_bindings(p, source, bindings);
            }
        }
        // 関数宣言 / named function・class 式の名前も値 binding
        // (`function Alpha() {} Alpha.fmt()` は関数オブジェクトへの access)。
        // class_declaration は collect_local_class_definitions が owner 一致時に
        // 正票の源として登録するため対象外。named class **式** の名前は内部スコープ
        // 限定なので、同名の外部 access を owner へ誤帰属しないようこちらで登録する。
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "generator_function"
        | "class" => {
            if let Some(n) = node.child_by_field_name("name")
                && n.kind() == "identifier"
                && let Ok(text) = n.utf8_text(source)
            {
                register_binding(bindings, text, BindingSpec::Unresolvable);
            }
        }
        _ => {}
    }
}

/// `type_annotation` 配下の型名。`Alpha` → `Class`、`Alpha[]` → `ArrayOf` (subscript access 用)。
fn type_annotation_ref<'a>(node: Node<'_>, source: &'a [u8]) -> Option<TypeRef<'a>> {
    let ty = node.named_child(0)?;
    match ty.kind() {
        "type_identifier" => Some(TypeRef::Class(ty.utf8_text(source).ok()?)),
        "array_type" => {
            let elem = ty.named_child(0)?;
            if elem.kind() != "type_identifier" {
                return None;
            }
            Some(TypeRef::ArrayOf(elem.utf8_text(source).ok()?))
        }
        _ => None,
    }
}

/// `new Alpha(...)` の constructor 名 (identifier のときだけ)。
fn new_expression_class<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    if node.kind() != "new_expression" {
        return None;
    }
    let ctor = node.child_by_field_name("constructor")?;
    if ctor.kind() != "identifier" {
        return None;
    }
    ctor.utf8_text(source).ok()
}

/// クラス名テキストを owner へ解決する。import (alias 含む) または同一ファイル内
/// class 定義に限定する — 単なる owner 名一致での確定は `const Alpha = getBeta();`
/// のような owner と同名の変数を class と誤認するため行わない。
fn resolve_class_name<'a>(
    text: &str,
    local_to_owner: &HashMap<&'a str, &'a str>,
) -> Option<&'a str> {
    local_to_owner.get(text).copied()
}

/// ノードを囲む class 宣言の名前を返す。
fn enclosing_class_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "class_declaration" | "class") {
            return n
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok());
        }
        cur = n.parent();
    }
    None
}

/// 推定不能な access (computed property / 文字列リテラル) が含まれるかをバイトレベルで検出する。
fn contains_ambiguous_member_token(source: &[u8], bare: &str) -> bool {
    let dq = format!("\"{bare}\"");
    let sq = format!("'{bare}'");
    let bt = format!("`{bare}`");
    memchr::memmem::find(source, dq.as_bytes()).is_some()
        || memchr::memmem::find(source, sq.as_bytes()).is_some()
        || memchr::memmem::find(source, bt.as_bytes()).is_some()
}

fn ts_language_for(lang: LangId) -> Option<tree_sitter::Language> {
    if lang.is_lexer_only() {
        return None;
    }
    Some(lang.ts_language())
}

fn is_js_ts_lang(lang: LangId) -> bool {
    matches!(lang, LangId::Typescript | LangId::Javascript | LangId::Tsx)
}

/// import 文から owner 名を検出する query。
/// `import_specifier` の `name` / `alias` / namespace を細かく分けて capture する。
const IMPORT_QUERY: &str = r#"
(import_statement
  (import_clause
    [
      (identifier) @default_name
      (named_imports
        (import_specifier
          name: (identifier) @import_name
          alias: (identifier)? @import_alias))
      (namespace_import
        (identifier) @namespace_name)
    ]))
"#;

/// `obj.member` および `obj?.member` の右辺 property_identifier を捕捉する query。
/// 分割代入 `{ member } = obj` は `DESTRUCTURED_KEY_QUERY`、`obj["member"]` は
/// 引用符付きトークンの検出 (`contains_ambiguous_member_token`) で別途 ambiguous に倒す。
const MEMBER_ACCESS_QUERY: &str = r#"
(member_expression
  property: (property_identifier) @prop)
"#;

/// 分割代入パターンのキー位置にあるメンバー名を捕捉する query。変数宣言・パラメータ・
/// 代入式・for-of の left のいずれでも `object_pattern` 配下に同じ形で現れる。
/// shorthand (`{ m }`) / 別名 (`{ m: alias }`) / default 付き (`{ m = d }`) の 3 形。
/// 文字列キー (`{ "m": a }`) は引用符付きトークンの検出側が拾う。
const DESTRUCTURED_KEY_QUERY: &str = r#"
(object_pattern (shorthand_property_identifier_pattern) @key)
(object_pattern (pair_pattern key: (property_identifier) @key))
(object_pattern (object_assignment_pattern left: (shorthand_property_identifier_pattern) @key))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str, lang: LangId, owners: &[&str], member: &str) -> FileAnalysis {
        let tree = parser::parse_source(src.as_bytes(), lang).unwrap();
        FileFacts::collect(tree.root_node(), src.as_bytes(), lang).analyze(owners, member)
    }

    fn count_for(analysis: &FileAnalysis, owner: &str) -> usize {
        analysis.resolved_counts.get(owner).copied().unwrap_or(0)
    }

    /// 対照: import + 素の static access は owner へ票が入る。
    #[test]
    fn plain_static_access_resolves_to_owner() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nAlpha.fmt();\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 1);
    }

    /// for-of の宣言付き loop 変数は owner クラス名を shadow する:
    /// `Alpha.fmt()` を import 由来の static access と誤認して票を入れない。
    #[test]
    fn for_of_loop_variable_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nimport { Beta } from \"./beta\";\nexport function run(items: Beta[]) {\n  for (const Alpha of items) {\n    Alpha.fmt();\n  }\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(
            a.has_unresolved_access,
            "loop 変数 shadow は不解決に倒すべき"
        );
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// kind の無い `for (x of xs)` は代入であって新規 binding ではない:
    /// import owner への static access は引き続き票が入る (過剰 shadow しない)。
    #[test]
    fn for_of_assignment_left_does_not_shadow() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nlet x: number;\nexport function run(xs: number[]) {\n  for (x of xs) {\n    Alpha.fmt();\n  }\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 1);
    }

    /// catch パラメータも owner 名を shadow する。
    #[test]
    fn catch_parameter_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run() {\n  try {\n    work();\n  } catch (Alpha) {\n    Alpha.fmt();\n  }\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(
            a.has_unresolved_access,
            "catch param shadow は不解決に倒すべき"
        );
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// bare arrow パラメータ (`Alpha => ...`) も shadow する。
    #[test]
    fn bare_arrow_parameter_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport const run = (xs: unknown[]) => xs.map(Alpha => Alpha.fmt());\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(
            a.has_unresolved_access,
            "bare arrow param shadow は不解決に倒すべき"
        );
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// destructuring binding (`const { Alpha } = ...`) も shadow する。
    #[test]
    fn destructuring_binding_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run(bag: { Alpha: unknown }) {\n  const { Alpha } = bag;\n  Alpha.fmt();\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(
            a.has_unresolved_access,
            "destructuring shadow は不解決に倒すべき"
        );
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// JS の bare 関数パラメータ (formal_parameters 直下 identifier) も shadow する。
    #[test]
    fn js_bare_function_parameter_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run(Alpha) {\n  Alpha.fmt();\n}\n",
            LangId::Javascript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(
            a.has_unresolved_access,
            "JS bare param shadow は不解決に倒すべき"
        );
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// 関数宣言名 (`function Alpha() {}`) は関数オブジェクトへの access であり
    /// owner クラスへの票にしない。
    #[test]
    fn function_declaration_name_shadow_is_unresolved() {
        let a = analyze(
            "import { Beta } from \"./beta\";\nfunction Alpha() {}\nAlpha.fmt();\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(
            a.has_unresolved_access,
            "関数宣言名 shadow は不解決に倒すべき"
        );
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// 冗長括弧付き receiver (`(new Alpha()).fmt()` / `(alpha).fmt()`) は括弧を
    /// 剥がして実 receiver で解決する (Ambiguous への過剰倒れを防ぐ)。
    #[test]
    fn parenthesized_receiver_is_unwrapped() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\n(new Alpha()).fmt();\nconst alpha: Alpha = new Alpha();\n(alpha).fmt();\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 2);
    }

    /// 分割代入でメンバーを取り出す access は member_expression にならないため、旧実装は
    /// 見落として `Alpha.parse` / `Beta.parse` を両方 dead にしていた。キー位置に同名が
    /// 現れたら不解決 (duplicate set 全体を Ambiguous) に倒す。
    /// TS / TSX / JS の 3 文法で同じ query が効くこと (query のノード名が文法に無いと
    /// コンパイル失敗で黙って判定が抜ける) も固定する。
    #[test]
    fn destructured_member_key_is_unresolved() {
        let forms = [
            "const { parse } = Alpha;\nparse(\"x\");\n",
            "const { parse: p } = Alpha;\np(\"x\");\n",
            "const { parse = fallback } = Alpha;\nparse(\"x\");\n",
            "export function f({ parse }) { return parse(\"x\"); }\nf(Alpha);\n",
            "let parse;\n({ parse } = Alpha);\nparse(\"x\");\n",
            "for (const { parse } of [Alpha]) { parse(\"x\"); }\n",
            "const { nested: { parse } } = holder;\nparse(\"x\");\n",
        ];
        for lang in [LangId::Typescript, LangId::Tsx, LangId::Javascript] {
            for body in forms {
                let src = format!("import {{ Alpha }} from \"./alpha\";\n{body}");
                let a = analyze(&src, lang, &["Alpha", "Beta"], "parse");
                assert!(
                    a.has_unresolved_access,
                    "{lang:?}: 分割代入の取り出しは不解決に倒すべき: {body}"
                );
            }
        }
    }

    /// 対照: キーが別名の分割代入 (`{ other }`) や、値側の束縛名が偶然同名なだけの
    /// パターン (`{ other: parse }`) は対象メンバーの取り出しではないので不解決にしない。
    /// 通常の static access は従来どおり owner へ票が入る。
    #[test]
    fn unrelated_destructuring_does_not_mark_member_unresolved() {
        for lang in [LangId::Typescript, LangId::Tsx, LangId::Javascript] {
            let a = analyze(
                "import { Alpha } from \"./alpha\";\nconst { other } = Alpha;\nconst { other: parse } = Alpha;\nAlpha.parse(\"x\");\n",
                lang,
                &["Alpha", "Beta"],
                "parse",
            );
            assert!(!a.has_unresolved_access, "{lang:?}");
            assert_eq!(count_for(&a, "Alpha"), 1, "{lang:?}");
        }
    }

    /// 型注釈付きの単純 identifier は従来どおり owner へ解決される (回帰確認)。
    #[test]
    fn typed_variable_still_resolves_to_owner() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run(alpha: Alpha) {\n  alpha.fmt();\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 1);
    }

    /// 修正前の再帰版 (浅い入力でだけ使う参照実装)。
    fn collect_local_class_definitions_recursive<'a>(
        node: Node<'a>,
        source: &'a [u8],
        class_names: &mut Vec<(&'a str, &'a str)>,
    ) {
        if matches!(node.kind(), "class_declaration" | "class")
            && let Some(name) = node.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
        {
            class_names.push((text, text));
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_local_class_definitions_recursive(child, source, class_names);
        }
    }

    /// 修正前の再帰版。1 ノード分の処理は共通で、辿る順序だけを再帰で再現する。
    fn collect_binding_facts_recursive<'a>(
        node: Node<'a>,
        source: &'a [u8],
        bindings: &mut HashMap<&'a str, BindingFact<'a>>,
    ) {
        collect_binding_facts_at(node, source, bindings);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_binding_facts_recursive(child, source, bindings);
        }
    }

    fn describe_binding_facts(
        bindings: &HashMap<&str, BindingFact<'_>>,
    ) -> std::collections::BTreeMap<String, String> {
        let describe_type = |ty: Option<TypeRef<'_>>| match ty {
            Some(TypeRef::Class(class)) => format!("class:{class}"),
            Some(TypeRef::ArrayOf(class)) => format!("array:{class}"),
            None => "-".to_string(),
        };
        bindings
            .iter()
            .map(|(name, fact)| {
                let value = match fact {
                    BindingFact::Multiple => "multiple".to_string(),
                    BindingFact::Single(BindingSpec::Unresolvable) => "unresolvable".to_string(),
                    BindingFact::Single(BindingSpec::Typed {
                        annotation,
                        constructor,
                    }) => format!(
                        "typed:{}:{}",
                        describe_type(*annotation),
                        constructor.unwrap_or("-")
                    ),
                };
                (name.to_string(), value)
            })
            .collect()
    }

    /// class 定義と束縛の収集は、反復版と再帰版で同じ結果になる。
    #[test]
    fn collectors_match_recursive_version() {
        let sources = [
            "import { Alpha } from \"./alpha\";\nclass Beta { fmt() { return 1; } }\nconst a = new Alpha();\nconst b: Beta = make();\nconst xs: Alpha[] = [];\nexport function run(p: Alpha, q, { r, s: [t] }) {\n  for (const u of xs) { try { u.fmt(); } catch (e) { const a = 1; } }\n  const f = v => v;\n  return (function g() { class Alpha {} return p; })();\n}\n",
            "function Alpha() {}\nconst k = class Beta {};\nfor (const [m, n = 1] of []) {}\nconst { o = new Alpha(), ...rest } = {};\n",
        ];
        for lang in [LangId::Typescript, LangId::Javascript] {
            for source in sources {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                let root = tree.root_node();

                let mut classes = Vec::new();
                collect_local_class_definitions(root, source.as_bytes(), &mut classes);
                let mut expected_classes = Vec::new();
                collect_local_class_definitions_recursive(
                    root,
                    source.as_bytes(),
                    &mut expected_classes,
                );
                assert_eq!(classes, expected_classes, "{lang:?}: {source}");
                assert!(!classes.is_empty(), "{lang:?}: class 定義を集めている");

                let mut bindings = HashMap::new();
                collect_binding_facts(root, source.as_bytes(), &mut bindings);
                let mut expected_bindings = HashMap::new();
                collect_binding_facts_recursive(root, source.as_bytes(), &mut expected_bindings);
                assert_eq!(
                    describe_binding_facts(&bindings),
                    describe_binding_facts(&expected_bindings),
                    "{lang:?}: {source}"
                );
                assert!(!bindings.is_empty(), "{lang:?}: 束縛を集めている");
            }
        }
    }

    /// 変更前の実装 (64bfb37)。duplicate set ごとに木全体を走査し、owner 集合で絞りながら
    /// クラス名・束縛の表を作って receiver を分類していた。`FileFacts` との比較だけに使う。
    #[allow(clippy::all)]
    mod reference {
        use super::super::*;

        /// ローカル変数 / パラメータの owner 束縛。
        enum VarBinding {
            /// `const a = new Alpha()` / `a: Alpha` のように単一 owner へ辿れる。
            Owner(String),
            /// `const xs: Alpha[] = ...` のように owner の配列型。`xs[i].member` で owner に辿れる。
            ElementOwner(String),
            /// factory 戻り値・非 owner 型・同名バインディング複数などで辿れない。
            Unresolvable,
        }

        pub(super) fn analyze_file(
            root: tree_sitter::Node<'_>,
            source: &[u8],
            target_owners: &[&str],
            target_member: &str,
            lang: LangId,
        ) -> FileAnalysis {
            let mut analysis = FileAnalysis {
                resolved_counts: HashMap::new(),
                has_unresolved_access: false,
            };
            let owners_set: HashSet<&str> = target_owners.iter().copied().collect();

            // 1. クラス名として使えるローカル名 → owner のマップを作る。
            //    - import (default / named / alias): `import { Alpha as A }` → A → Alpha
            //    - 同ファイル内 class 定義: class Alpha {} → Alpha → Alpha
            //    `import * as ns` 経由のアクセス (`ns.Alpha.fmt()`) は receiver が
            //    member_expression になり通常の分類で不解決に落ちるため、namespace import の
            //    存在だけでファイル全体を Ambiguous にはしない (対象 member への access が
            //    無いファイルまで巻き込むのを避ける)。
            let mut local_to_owner: HashMap<String, String> = HashMap::new();
            if ts_language_for(lang).is_some()
                && let Ok(query) = crate::engine::query_cache::cached_query(lang, IMPORT_QUERY)
            {
                let mut cursor = QueryCursor::new();
                let mut matches = cursor.matches(&query, root, source);
                while let Some(m) = matches.next() {
                    let mut import_name: Option<&str> = None;
                    let mut import_alias: Option<&str> = None;
                    for cap in m.captures() {
                        let Ok(text) = cap.node.utf8_text(source) else {
                            continue;
                        };
                        let cap_name = &query.capture_names()[cap.index as usize];
                        match *cap_name {
                            "default_name" => {
                                if owners_set.contains(text) {
                                    local_to_owner.insert(text.to_string(), text.to_string());
                                }
                            }
                            "import_name" => import_name = Some(text),
                            "import_alias" => import_alias = Some(text),
                            _ => {}
                        }
                    }
                    if let Some(name) = import_name
                        && owners_set.contains(name)
                    {
                        let local = import_alias.unwrap_or(name);
                        local_to_owner.insert(local.to_string(), name.to_string());
                    }
                }
            }
            collect_local_class_definitions(root, source, &owners_set, &mut local_to_owner);

            // 2. ローカル変数 / パラメータの owner 束縛マップ。
            let mut var_bindings: HashMap<String, VarBinding> = HashMap::new();
            collect_var_bindings(
                root,
                source,
                &local_to_owner,
                &owners_set,
                &mut var_bindings,
            );

            // 3. `obj.member` / `obj?.member` の receiver を分類して票を入れる。
            if ts_language_for(lang).is_some()
                && let Ok(query) =
                    crate::engine::query_cache::cached_query(lang, MEMBER_ACCESS_QUERY)
            {
                let mut cursor = QueryCursor::new();
                let mut matches = cursor.matches(&query, root, source);
                while let Some(m) = matches.next() {
                    for cap in m.captures() {
                        let node = cap.node;
                        let Ok(text) = node.utf8_text(source) else {
                            continue;
                        };
                        if text != target_member {
                            continue;
                        }
                        let Some(member_expr) = node.parent() else {
                            continue;
                        };
                        match resolve_member_receiver(
                            member_expr,
                            source,
                            &local_to_owner,
                            &var_bindings,
                            &owners_set,
                        ) {
                            Some(owner) => {
                                *analysis.resolved_counts.entry(owner).or_default() += 1;
                            }
                            None => {
                                analysis.has_unresolved_access = true;
                                return analysis;
                            }
                        }
                    }
                }
            }

            // 4. 分割代入でメンバーを取り出す access (`const { parse } = Alpha` /
            //    `function f({ parse })` / `({ parse } = x)`) は member_expression にならないため
            //    3 の query に掛からず、分割代入でしか使われない同名メンバーが両方 dead に出ていた。
            //    右辺の receiver を辿れたとしても取り出した値の行き先は追わないので、帰属を推測せず
            //    パターンのキーに同名が現れた時点で不解決 (duplicate set 全体を Ambiguous) に倒す。
            if ts_language_for(lang).is_some()
                && let Ok(query) =
                    crate::engine::query_cache::cached_query(lang, DESTRUCTURED_KEY_QUERY)
            {
                let mut cursor = QueryCursor::new();
                let mut matches = cursor.matches(&query, root, source);
                while let Some(m) = matches.next() {
                    if m.captures()
                        .iter()
                        .any(|cap| cap.node.utf8_text(source) == Ok(target_member))
                    {
                        analysis.has_unresolved_access = true;
                        return analysis;
                    }
                }
            }

            analysis
        }

        /// 同ファイル内の class 定義名 (owner に一致するもの) をクラス名マップへ加える。
        ///
        /// ファイル全体を辿るため、子 1 段ごとに再帰すると深い配列・式を持つファイル 1 つで
        /// dead-code ごと落ちる。named_children の再帰と同じ順序で反復して辿る。
        fn collect_local_class_definitions(
            root: tree_sitter::Node<'_>,
            source: &[u8],
            owners_set: &HashSet<&str>,
            local_to_owner: &mut HashMap<String, String>,
        ) {
            let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
                if matches!(node.kind(), "class_declaration" | "class")
                    && let Some(name) = node.child_by_field_name("name")
                    && let Ok(text) = name.utf8_text(source)
                    && owners_set.contains(text)
                {
                    local_to_owner.insert(text.to_string(), text.to_string());
                }
                ControlFlow::Continue(Walk::Descend)
            });
        }

        /// binding マップへ 1 件登録する。同名バインディングが複数回現れた場合は
        /// スコープ解決をせず `Unresolvable` に落とす (shadow / 別スコープの一意解決を諦める)。
        fn register_binding(
            bindings: &mut HashMap<String, VarBinding>,
            name: String,
            value: VarBinding,
        ) {
            use std::collections::hash_map::Entry;
            match bindings.entry(name) {
                Entry::Vacant(e) => {
                    e.insert(value);
                }
                Entry::Occupied(mut e) => {
                    e.insert(VarBinding::Unresolvable);
                }
            }
        }

        /// binding パターン (identifier / destructuring / rest / default) 配下の束縛名を
        /// すべて `Unresolvable` として登録する。中身の型は静的に辿れないため owner 推論は
        /// しない。収集漏れは shadow 見逃し → import owner への誤帰属 (fail-open) になる
        /// ため、未知の構造は全 named children を辿って leaf の identifier を漏らさない。
        fn register_pattern_bindings(
            pattern: tree_sitter::Node<'_>,
            source: &[u8],
            bindings: &mut HashMap<String, VarBinding>,
        ) {
            match pattern.kind() {
                "identifier" | "shorthand_property_identifier_pattern" => {
                    if let Ok(name) = pattern.utf8_text(source) {
                        register_binding(bindings, name.to_string(), VarBinding::Unresolvable);
                    }
                }
                // `{ key: alias }` の key 側は binding ではないため value 側のみ辿る。
                "pair_pattern" => {
                    if let Some(v) = pattern.child_by_field_name("value") {
                        register_pattern_bindings(v, source, bindings);
                    }
                }
                // default 値の右辺 (`{ a = expr }` / `(x = expr)`) は参照であって binding
                // ではないため left のみ辿る。
                "assignment_pattern" | "object_assignment_pattern" => {
                    if let Some(l) = pattern.child_by_field_name("left") {
                        register_pattern_bindings(l, source, bindings);
                    }
                }
                _ => {
                    let mut cursor = pattern.walk();
                    for child in pattern.named_children(&mut cursor) {
                        register_pattern_bindings(child, source, bindings);
                    }
                }
            }
        }

        /// `variable_declarator` / パラメータの型注釈から変数 → owner の束縛を集める。
        ///
        /// owner への静的解決を試みるのは「単純 identifier + 型注釈 / `new` 初期化子」の
        /// 経路のみ。それ以外の binding 導入構文 (destructuring / for-of・for-in の宣言付き
        /// loop 変数 / catch / bare arrow・JS パラメータ / 関数宣言・named 式の名前) は
        /// `Unresolvable` として登録する — 収集しないと receiver 解決が import 由来の
        /// class static access へフォールバックし、shadow された名前の票を owner へ誤計上
        /// する (dead-code の fail-open)。
        ///
        /// ファイル全体を辿るので、named_children の再帰と同じ順序で反復して辿る
        /// (深い配列・式を持つファイル 1 つで dead-code ごと落ちていた)。
        fn collect_var_bindings(
            root: tree_sitter::Node<'_>,
            source: &[u8],
            local_to_owner: &HashMap<String, String>,
            owners_set: &HashSet<&str>,
            bindings: &mut HashMap<String, VarBinding>,
        ) {
            let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
                collect_var_bindings_at(node, source, local_to_owner, owners_set, bindings);
                ControlFlow::Continue(Walk::Descend)
            });
        }

        /// `collect_var_bindings` の 1 ノード分の処理 (子へは降りない)。
        fn collect_var_bindings_at(
            node: tree_sitter::Node<'_>,
            source: &[u8],
            local_to_owner: &HashMap<String, String>,
            owners_set: &HashSet<&str>,
            bindings: &mut HashMap<String, VarBinding>,
        ) {
            match node.kind() {
                "variable_declarator" => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        if name_node.kind() == "identifier" {
                            if let Ok(name) = name_node.utf8_text(source) {
                                // 型注釈が最優先 (`const a: Alpha = ...`)、次に `= new Alpha()`。
                                let owner = node
                                    .child_by_field_name("type")
                                    .and_then(|t| {
                                        type_annotation_owner(t, source, local_to_owner, owners_set)
                                    })
                                    .or_else(|| {
                                        node.child_by_field_name("value")
                                            .and_then(|v| {
                                                new_expression_owner(
                                                    v,
                                                    source,
                                                    local_to_owner,
                                                    owners_set,
                                                )
                                            })
                                            .map(VarBinding::Owner)
                                    });
                                register_binding(
                                    bindings,
                                    name.to_string(),
                                    owner.unwrap_or(VarBinding::Unresolvable),
                                );
                            }
                        } else {
                            // destructuring (`const { a } = ...`) は要素の型を辿れない。
                            register_pattern_bindings(name_node, source, bindings);
                        }
                    }
                }
                "required_parameter" | "optional_parameter" => {
                    if let Some(pat) = node.child_by_field_name("pattern") {
                        if pat.kind() == "identifier" {
                            if let Ok(name) = pat.utf8_text(source) {
                                let owner = node.child_by_field_name("type").and_then(|t| {
                                    type_annotation_owner(t, source, local_to_owner, owners_set)
                                });
                                register_binding(
                                    bindings,
                                    name.to_string(),
                                    owner.unwrap_or(VarBinding::Unresolvable),
                                );
                            }
                        } else {
                            register_pattern_bindings(pat, source, bindings);
                        }
                    }
                }
                // JS の関数パラメータは formal_parameters 直下に bare pattern で並ぶ
                // (TS は required/optional_parameter に包まれ上の arm が処理する)。
                "formal_parameters" => {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if !matches!(child.kind(), "required_parameter" | "optional_parameter") {
                            register_pattern_bindings(child, source, bindings);
                        }
                    }
                }
                // arrow の単独パラメータ (`x => ...`) は formal_parameters に包まれない。
                "arrow_function" => {
                    if let Some(p) = node.child_by_field_name("parameter") {
                        register_pattern_bindings(p, source, bindings);
                    }
                }
                // for-of / for-in の宣言付き loop 変数 (`for (const x of xs)`)。left は
                // lexical_declaration に包まれない bare pattern のため専用処理が要る。
                // kind (var/let/const) の無い `for (x of xs)` は既存変数への代入であって
                // 新規 binding ではないため対象外。
                "for_in_statement" => {
                    if node.child_by_field_name("kind").is_some()
                        && let Some(left) = node.child_by_field_name("left")
                    {
                        register_pattern_bindings(left, source, bindings);
                    }
                }
                // catch (e) の e。
                "catch_clause" => {
                    if let Some(p) = node.child_by_field_name("parameter") {
                        register_pattern_bindings(p, source, bindings);
                    }
                }
                // 関数宣言 / named function・class 式の名前も値 binding
                // (`function Alpha() {} Alpha.fmt()` は関数オブジェクトへの access)。
                // class_declaration は collect_local_class_definitions が owner 一致時に
                // 正票の源として登録するため対象外。named class **式** の名前は内部スコープ
                // 限定なので、同名の外部 access を owner へ誤帰属しないようこちらで登録する。
                "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "generator_function"
                | "class" => {
                    if let Some(n) = node.child_by_field_name("name")
                        && n.kind() == "identifier"
                        && let Ok(text) = n.utf8_text(source)
                    {
                        register_binding(bindings, text.to_string(), VarBinding::Unresolvable);
                    }
                }
                _ => {}
            }
        }

        /// `type_annotation` 配下の型名を owner へ解決する。
        /// `Alpha` → `Owner`、`Alpha[]` → `ElementOwner` (subscript access 用)。
        fn type_annotation_owner(
            node: tree_sitter::Node<'_>,
            source: &[u8],
            local_to_owner: &HashMap<String, String>,
            owners_set: &HashSet<&str>,
        ) -> Option<VarBinding> {
            let ty = node.named_child(0)?;
            match ty.kind() {
                "type_identifier" => {
                    let text = ty.utf8_text(source).ok()?;
                    resolve_class_name(text, local_to_owner, owners_set).map(VarBinding::Owner)
                }
                "array_type" => {
                    let elem = ty.named_child(0)?;
                    if elem.kind() != "type_identifier" {
                        return None;
                    }
                    let text = elem.utf8_text(source).ok()?;
                    resolve_class_name(text, local_to_owner, owners_set)
                        .map(VarBinding::ElementOwner)
                }
                _ => None,
            }
        }

        /// `new Alpha(...)` の constructor 名を owner へ解決する。
        fn new_expression_owner(
            node: tree_sitter::Node<'_>,
            source: &[u8],
            local_to_owner: &HashMap<String, String>,
            owners_set: &HashSet<&str>,
        ) -> Option<String> {
            if node.kind() != "new_expression" {
                return None;
            }
            let ctor = node.child_by_field_name("constructor")?;
            if ctor.kind() != "identifier" {
                return None;
            }
            let text = ctor.utf8_text(source).ok()?;
            resolve_class_name(text, local_to_owner, owners_set)
        }

        /// クラス名テキストを owner へ解決する。import (alias 含む) または同一ファイル内
        /// class 定義に限定する — 単なる owner 名一致での確定は `const Alpha = getBeta();`
        /// のような owner と同名の変数を class と誤認するため行わない。
        fn resolve_class_name(
            text: &str,
            local_to_owner: &HashMap<String, String>,
            _owners_set: &HashSet<&str>,
        ) -> Option<String> {
            local_to_owner.get(text).cloned()
        }

        /// `member_expression` の receiver (object 側) を owner へ静的に解決する。
        /// 解決対象: owner 名の static access / `new Owner().m()` / owner 束縛済み変数 /
        /// class 内 `this.m()` (enclosing class が owner の場合)。それ以外は `None` (不解決)。
        fn resolve_member_receiver(
            member_expr: tree_sitter::Node<'_>,
            source: &[u8],
            local_to_owner: &HashMap<String, String>,
            var_bindings: &HashMap<String, VarBinding>,
            owners_set: &HashSet<&str>,
        ) -> Option<String> {
            let mut object = member_expr.child_by_field_name("object")?;
            // 冗長括弧 (`(new Owner()).member` / `(alpha).member`) は剥がして実 receiver で
            // 判定する (剥がさないと Ambiguous に落ちて duplicate set の検出力が下がる)。
            while object.kind() == "parenthesized_expression" {
                object = object.named_child(0)?;
            }
            match object.kind() {
                "identifier" => {
                    let text = object.utf8_text(source).ok()?;
                    // 同名のローカル binding があれば shadow とみなし変数として判定する
                    // (`const Alpha = getBeta(); Alpha.fmt()` を class static access と
                    // 誤認しない)。binding が無い場合のみ import / 同一ファイル class 名。
                    if let Some(binding) = var_bindings.get(text) {
                        return match binding {
                            VarBinding::Owner(owner) => Some(owner.clone()),
                            _ => None,
                        };
                    }
                    resolve_class_name(text, local_to_owner, owners_set)
                }
                "new_expression" => {
                    new_expression_owner(object, source, local_to_owner, owners_set)
                }
                // `xs[i].member`: xs が owner 配列型 (`Alpha[]`) に束縛されていれば要素は owner。
                "subscript_expression" => {
                    let array = object.child_by_field_name("object")?;
                    if array.kind() != "identifier" {
                        return None;
                    }
                    let text = array.utf8_text(source).ok()?;
                    match var_bindings.get(text) {
                        Some(VarBinding::ElementOwner(owner)) => Some(owner.clone()),
                        _ => None,
                    }
                }
                "this" => {
                    let class_name = enclosing_class_name(member_expr, source)?;
                    if owners_set.contains(class_name.as_str()) {
                        Some(class_name)
                    } else {
                        // enclosing class が owner でない `this.m()` は継承経由で owner へ
                        // 到達し得るため、推測せず不解決に倒す。
                        None
                    }
                }
                _ => None,
            }
        }

        /// ノードを囲む class 宣言の名前を返す。
        fn enclosing_class_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
            let mut cur = node.parent();
            while let Some(n) = cur {
                if matches!(n.kind(), "class_declaration" | "class") {
                    return n
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                        .map(str::to_string);
                }
                cur = n.parent();
            }
            None
        }
    }

    fn summarize(analysis: &FileAnalysis) -> (Vec<(String, usize)>, bool) {
        let mut counts: Vec<(String, usize)> = analysis
            .resolved_counts
            .iter()
            .map(|(owner, count)| (owner.clone(), *count))
            .collect();
        counts.sort();
        (counts, analysis.has_unresolved_access)
    }

    /// ファイルごとに 1 回だけ集めた事実から set ごとに出す結果は、set ごとに木全体を
    /// 走査していた変更前の実装と同じになる (owner 集合に依存する解決を共有していないこと)。
    /// import の別名・同ファイルの class (後勝ち)・型注釈と `new` の優先順・同名の束縛・
    /// `this`・配列要素・括弧・分割代入のキーを、owner 集合と member を変えて突き合わせる。
    #[test]
    fn file_facts_match_per_set_reference() {
        let sources = [
            // import (default / named / alias / namespace) と static access
            "import Alpha from \"./a\";\nimport { Beta as B, Gamma } from \"./b\";\nimport * as ns from \"./c\";\nAlpha.fmt();\nB.fmt();\nGamma.label();\nns.Alpha.fmt();\n",
            // 別名 import と同名の class 定義 (class が後勝ち)
            "import { Beta as Alpha } from \"./b\";\nclass Alpha { fmt() { return this.fmt(); } }\nAlpha.fmt();\n",
            // 型注釈 → new の順、注釈が owner でなければ new へ、配列型と添字
            "import { Alpha, Beta } from \"./ab\";\nconst a: Alpha = make();\na.fmt();\nconst b: Unknown = new Beta();\nb.fmt();\nconst c: Alpha = new Beta();\nc.fmt();\nconst xs: Alpha[] = [];\nxs[0].fmt();\nlet ys: Beta[];\nys[1].label();\nconst zs = [];\nzs[0].fmt();\n",
            // 同名の束縛が 2 つ (辿れない) と shadow
            "import { Alpha, Beta } from \"./ab\";\nconst c = new Alpha();\nfunction f() { const c = new Beta(); return c; }\nc.fmt();\nconst Gamma = getBeta();\nGamma.fmt();\n",
            // パラメータ・括弧・optional chaining・呼び出し結果
            "import { Alpha, Beta } from \"./ab\";\nexport function g(p: Alpha, q?: Beta, r?: Gamma) {\n  p.fmt();\n  q?.fmt();\n  (p).label();\n  (new Beta()).fmt();\n  make().label();\n  r.fmt();\n}\n",
            // this (owner の class / owner でない class / 無名 class / class の外)
            "class Alpha { m() { this.fmt(); [1].map(() => this.label()); } }\nclass Gamma { m() { this.fmt(); } }\nconst K = class Beta { m() { this.fmt(); } };\nconst Q = class { m() { this.label(); } };\nfunction outer() { this.fmt(); }\n",
            // 分割代入のキーと、値側だけが同名のパターン
            "import { Alpha } from \"./alpha\";\nconst { fmt } = Alpha;\nconst { other: label } = Alpha;\nAlpha.label();\nconst { parse = fallback } = holder;\n",
            // JS の bare パラメータ・catch・for-of・関数名・class 式の名前
            "import { Alpha } from \"./alpha\";\nexport function run(Alpha, { Beta }) {\n  Alpha.fmt();\n  Beta.fmt();\n}\ntry { x(); } catch (Gamma) { Gamma.fmt(); }\nfor (const B of []) { B.label(); }\nfunction Beta() {}\nBeta.label();\n",
            // 入れ子の分割代入 (別名・default・rest) による shadow
            "import { Alpha, Beta } from \"./ab\";\nconst { a: [Other, { b: Beta }] } = bag;\nBeta.fmt();\nconst [[Gamma = new Alpha()], ...rest] = xs;\nGamma.fmt();\nrest.label();\nfunction h({ p: { q: [Alpha] } }) { Alpha.fmt(); }\nAlpha.label();\n",
        ];
        let owner_sets: [&[&str]; 5] = [
            &["Alpha", "Beta"],
            &["Alpha", "Gamma"],
            &["Beta", "Gamma"],
            &["Alpha", "B"],
            &["Alpha", "Beta", "Gamma"],
        ];
        let members = ["fmt", "label", "parse", "m", "other"];
        let (mut resolved, mut unresolved, mut empty) = (0, 0, 0);
        for lang in [LangId::Typescript, LangId::Tsx, LangId::Javascript] {
            for source in sources {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                let root = tree.root_node();
                let facts = FileFacts::collect(root, source.as_bytes(), lang);
                for owners in owner_sets {
                    for member in members {
                        let actual = summarize(&facts.analyze(owners, member));
                        let expected = summarize(&reference::analyze_file(
                            root,
                            source.as_bytes(),
                            owners,
                            member,
                            lang,
                        ));
                        assert_eq!(
                            actual, expected,
                            "{lang:?} owners={owners:?} member={member}: {source}"
                        );
                        match actual {
                            (_, true) => unresolved += 1,
                            (counts, false) if !counts.is_empty() => resolved += 1,
                            _ => empty += 1,
                        }
                    }
                }
            }
        }
        // 空振り防止: 票が入る組・不解決になる組・何も起きない組がどれもある
        assert!(
            resolved > 0 && unresolved > 0 && empty > 0,
            "resolved={resolved} unresolved={unresolved} empty={empty}"
        );
    }
}
