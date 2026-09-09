#!/usr/bin/env node
/**
 * schema-compare.mjs —— REQ-008 Step 5：官方 dsh web 客户端 typert.remote-client.js 结构对比（SchemaDiff）
 *
 * 背景
 * - 官方 dsh web 客户端以 npm 包树发布各 controller 的 `typert.remote-client.js`
 *   （由 @deepseek-ai/dsh-typert-generator 从 Host FaceModel 生成）。
 * - 每个文件顶层为一批 `const X$schema = z.xxx(...)` zod codec 声明；
 *   其中“远程条目”的命名规则为：
 *     `_<pkg>_<ns>_<method>_parameter_<N>$schema`   —— 第 N 个入参（N 从 0 起）
 *     `_<pkg>_<ns>_<method>_result$schema`          —— 返回值
 *   pkg 段 = 包名 `@deepseek-ai/dsh-api-session-controller` 去掉 `@`、把 `/` 与 `-`
 *   替换为 `_`（如 `deepseek_ai_dsh_api_session_controller`）；namespace 为方法面段名
 *   （如 fileReferences/session/commands），method 为其后的调用名（均为 camelCase 单段）。
 * - 解析只做“正则 + 轻量括号配平”，不真正求值 zod：抽取每个条目的包络结构摘要
 *   （z.object({...}) 的对象 key 列表、z.union([...]) 成员数、z.array/z.record/z.literal、
 *   z.string/z.number/z.boolean 等原语、.optional()/.nullable()/.readonly()/.default() 修饰）。
 *
 * 产物（SchemaDiff JSON，schema_version 固定 1）
 * ```json
 * {
 *   "schema_version": 1,
 *   "from_version": "0.1.2-rc.1",
 *   "to_version": "0.2.0",
 *   "added":   [{ "namespace", "method", "kind": "parameter|result", "index"?, "path", "fields", "summary" }],
 *   "removed": [ 同上（取 from 侧摘要） ],
 *   "changed": [{ "namespace", "method", "kind", "index"?, "path", "from", "to", "change" }]
 * }
 * ```
 * - path 形式：`ns.method:param#N`（入参）或 `ns.method:result`（返回值）。
 * - added/removed/changed 均按 path 升序排列，输出确定、可重放。
 * - 条目内联结构：{ namespace, method, kind, index?, fields, summary }
 *   - fields：该条目包络内对象字段的有序列表（对象根直接列 key；
 *     array/union 根则取其元素/成员对象 key 集合，去重保序）。
 *   - summary：包络结构摘要，如 `object{sessionId,attachmentId}`、`array`、`union[3]`、
 *     `intersection[2]`、`string`、`string.optional` 等。
 * - 变更判定：两侧同一 (namespace, method, kind, index) 的条目，summary 或 fields
 *   任一不同 → changed[]。summary 描述“包络”形状（对象字段集合、union/intersection
 *   成员数、原语、修饰符）；fields 收集包络对象/数组元素/union 对象成员的字段名
 *   （数组可多层递归），因此“给数组元素对象加字段”这类变化也会被识别。
 *   注意：字段“类型”变化（如 string→number）与对象字段值内部更深层的结构调整
 *   不在当前摘要粒度内，不会触发 changed——如后续步骤需要可升 schema_version 细化。
 *
 * 用法
 * ```
 * # diff 模式（缺省）：对比两个版本（bundle 包树/单文件，或 schema 快照 JSON）
 * node scripts/schema-compare.mjs --from <dir-or-file> --to <dir-or-file> \
 *     [--from-version X] [--to-version Y] [--out path]
 * node scripts/schema-compare.mjs --from-snapshot <file> --to-snapshot <file> [--out path]
 *
 * # snapshot 模式：把一份 bundle 导出为规范 schema 快照 JSON（golden 入库用）
 * node scripts/schema-compare.mjs --mode snapshot --bundle <dir-or-file> \
 *     --version X --out schemas/dsh-api-schema-X.json
 * ```
 * - bundle 输入可以是单个 typert.remote-client.js 文件，也可以是包含此类文件的目录
 *   （递归收集所有 basename 以 `typert.remote-client.js` 结尾的文件）。
 * - 版本号（diff 的 bundle 侧）：优先 --from-version/--to-version；缺省读 package.json
 *   的 version（输入为包目录时读目录根 package.json；否则读首个 bundle 就近的
 *   package.json）；都取不到时为字符串 "unknown"。snapshot 侧版本内嵌于快照文件，
 *   不接受 --*-version 覆盖；snapshot 模式用 --version 显式指定（必填）。
 * - schema 快照（snapshot 模式产物 / --from-snapshot|--to-snapshot 输入）：
 *   与 diff 同一棵 namespace·method·field 解析树（复用 loadSide 收集 + 条目去重），
 *   序列化为确定格式 JSON——对象 key 全部字典序、namespace/method 名排序、parameter
 *   按 index 排序，因此同一输入两次运行字节一致（可作 commit 资产做跨版本 diff）。
 * - --out：原子写（同目录 `<path>.tmp` + rename，参照项目 config::atomic_write_0600
 *   同目录 rename 惯例；此处权限 0644 即可）。未指定时 SchemaDiff JSON 打到 stdout；
 *   snapshot 模式的 --out 必填（不做 stdout 导出）。
 * - 错误处理：输入路径不存在 / 无匹配文件 / 解析到零条 schema / bundle 语法无法解析
 *   （括号不平衡、pkg 前缀无法确定等）/ 快照 JSON 非法（schema_version ≠ 1、缺 version、
 *   结构坏、重复 path）→ stderr 明确报错、exit code 1，
 *   且不会产出半成品文件（所有解析与校验先于原子写完成）。成功 exit code 0。
 *
 * 运行约束：node ESM、顶层 await/import、零第三方依赖；模块同时可被
 * scripts/schema-compare.test.mjs import（按 import.meta 判定是否直接执行 CLI）。
 */

import * as fs from 'node:fs'
import * as path from 'node:path'
import { fileURLToPath } from 'node:url'

// ---------------------------------------------------------------------------
// 常量与基础工具
// ---------------------------------------------------------------------------

const BUNDLE_NAME_RE = /typert\.remote-client\.js$/ // basename 匹配（`*typert.remote-client.js`）

// ---------------------------------------------------------------------------
// 括号/字符串/注释 感知的轻量扫描
// ---------------------------------------------------------------------------

/**
 * 对一行文本更新跨行扫描状态（括号深度、字符串、块注释），返回新状态。
 * 不做配对校验，只累计；行注释 `//` 之后即行尾。
 */
function scanLine(line, state) {
  for (let i = 0; i < line.length; i++) {
    const ch = line[i]
    if (state.block) {
      if (ch === '*' && line[i + 1] === '/') { state.block = false; i++ }
      continue
    }
    if (state.quote) {
      if (ch === '\\') { i++ } // 跳过转义字符
      else if (ch === state.quote) state.quote = null
      continue
    }
    if (ch === '/' && line[i + 1] === '/') break // 行注释
    if (ch === '/' && line[i + 1] === '*') { state.block = true; i++; continue }
    if (ch === '"' || ch === "'" || ch === '`') { state.quote = ch; continue }
    if (ch === '(' || ch === '[' || ch === '{') state.depth++
    else if (ch === ')' || ch === ']' || ch === '}') state.depth--
  }
  return state
}

/**
 * 顶层声明切分：把 bundle 文本切成若干“顶层语句”，返回 [{name, parts[]}]。
 *
 * 关键点（真实 bundle 的格式陷阱）：
 * - 一条多行声明的延续行可能从第 0 列开始（如 union 成员边界 `}), z.object({`），
 *   因此“行首是 const”并不能作为新声明的边界；
 * - 判定边界需同时满足：该行开始时跨行嵌套深度为 0，且行首是
 *   const/let/var/export/import/function/class/return 或列 0 注释；
 * - 文件末尾还有 `export const TYPERT_REMOTE = {...}` 与 `export default`，
 *   它们同样是顶层语句，必须把上一个 $schema 声明截断掉。
 */
function splitDeclarations(text) {
  const lines = text.split('\n')
  const TOP_RE = /^(?:const|let|var|export|import|function|class|return)\b/
  const decls = [] // { name, parts }
  const state = { depth: 0, quote: null, block: false }
  let cur = null

  for (const line of lines) {
    const startDepth = state.depth
    const trimmed = line.trim()
    const isBoundary =
      startDepth === 0 &&
      (TOP_RE.test(trimmed) || trimmed.startsWith('//') || trimmed.startsWith('/*'))

    if (isBoundary && cur !== null) {
      decls.push(cur)
      cur = null
    }
    if (isBoundary) {
      const m = trimmed.match(/^const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(.*)$/)
      cur = m ? { name: m[1], parts: [m[2]] } : { name: null, parts: [] }
    } else if (cur !== null) {
      cur.parts.push(line) // 声明延续行（含第 0 列的 `})` 等）
    }
    scanLine(line, state)
  }
  if (cur !== null) decls.push(cur)

  // 全局括号/引号/注释必须配平
  if (state.depth !== 0 || state.quote !== null || state.block) {
    const why = state.depth !== 0 ? `括号不平衡(depth=${state.depth})`
      : state.quote !== null ? `字符串未闭合(${state.quote})`
        : '块注释未闭合'
    throw new Error(`bundle 语法无法解析：${why}`)
  }
  return decls.map((d) => ({ name: d.name, expr: d.parts.join('\n').trim() }))
}

/**
 * 从 openIdx（应为 `(`/`[`/`{`）扫描到配对的闭符号下标；失败返回 -1。
 * 全程字符串/注释感知。
 */
function findClosing(text, openIdx) {
  const open = text[openIdx]
  const closeOf = { '(': ')', '[': ']', '{': '}' }
  const wantClose = closeOf[open]
  if (!wantClose) return -1
  const stack = []
  let quote = null
  let block = false
  for (let i = openIdx; i < text.length; i++) {
    const ch = text[i]
    if (block) {
      if (ch === '*' && text[i + 1] === '/') { block = false; i++ }
      continue
    }
    if (quote) {
      if (ch === '\\') i++
      else if (ch === quote) quote = null
      continue
    }
    if (ch === '/' && text[i + 1] === '/') return -1 // 期望的闭符号不会出现在行注释后
    if (ch === '/' && text[i + 1] === '*') { block = true; i++; continue }
    if (ch === '"' || ch === "'" || ch === '`') { quote = ch; continue }
    if (ch === '(' || ch === '[' || ch === '{') { stack.push(ch); continue }
    if (ch === ')' || ch === ']' || ch === '}') {
      const top = stack.pop()
      if (top === undefined) return -1
      if ((top === '(' && ch !== ')') || (top === '[' && ch !== ']') || (top === '{' && ch !== '}')) return -1
      if (stack.length === 0) return i
    }
  }
  return -1
}

/**
 * 在 body（某对括号内部文本）里按“顶层逗号”切出条目。
 * 用于 z.union([...]) / z.enum([...]) 成员计数、z.intersection(a,b) 操作数计数。
 */
function splitTopLevelItems(body) {
  const items = []
  let depth = 0
  let quote = null
  let cur = []
  for (let i = 0; i < body.length; i++) {
    const ch = body[i]
    if (quote) {
      cur.push(ch)
      if (ch === '\\') { cur.push(body[i + 1] ?? ''); i++ }
      else if (ch === quote) quote = null
      continue
    }
    if (ch === '"' || ch === "'") { quote = ch; cur.push(ch); continue }
    if (ch === '(' || ch === '[' || ch === '{') { depth++; cur.push(ch); continue }
    if (ch === ')' || ch === ']' || ch === '}') { depth--; cur.push(ch); continue }
    if (ch === ',' && depth === 0) { items.push(cur.join('').trim()); cur = []; continue }
    cur.push(ch)
  }
  const tail = cur.join('').trim()
  if (tail !== '') items.push(tail)
  return items
}

/** 取括号体 argText 最外层 `[...]` 内的文本；没有方括号则原样返回。 */
function firstBracket(inner) {
  const t = inner.trim()
  if (t[0] !== '[') return t
  const c = findClosing(t, 0)
  if (c < 0) return t
  return t.slice(1, c)
}

// ---------------------------------------------------------------------------
// zod 结构摘要
// ---------------------------------------------------------------------------

/** 解析 z.object 参数文本，返回对象顶层 key 的有序列表；不是对象字面量时返回 null。 */
function objectKeys(argText) {
  const t = argText.trim()
  if (t[0] !== '{') return null
  const c = findClosing(t, 0)
  if (c < 0) return null
  const entries = splitTopLevelItems(t.slice(1, c))
  const keys = []
  for (const entry of entries) {
    const k = entryKey(entry)
    if (k !== null) keys.push(k)
  }
  return keys
}

/** 解析对象条目文本开头的 key（`'name': value` / `"name": ...` / `name: ...`）。 */
function entryKey(entry) {
  const t = entry.trim()
  if (t === '') return null
  const first = t[0]
  if (first === "'" || first === '"') {
    let s = ''
    let i = 1
    for (; i < t.length; i++) {
      const ch = t[i]
      if (ch === '\\') { s += t[i + 1] ?? ''; i++; continue }
      if (ch === first) { i++; break } // 越过闭合引号
      s += ch
    }
    while (i < t.length && /\s/.test(t[i])) i++
    return t[i] === ':' ? s : null
  }
  const m = t.match(/^([A-Za-z_$][A-Za-z0-9_$]*)\s*:/)
  return m ? m[1] : null
}

/** 解析后续修饰链 `.optional()/.nullable()/.readonly()/.default(...)` 等，按出现顺序返回名字。 */
function parseModifiers(rest) {
  const mods = []
  let s = rest
  for (;;) {
    const t = s.trimStart()
    const m = t.match(/^\.([A-Za-z_$][A-Za-z0-9_$]*)\s*\(/)
    if (!m) break
    const openIdx = t.indexOf('(')
    const c = findClosing(t, openIdx)
    if (c < 0) break
    mods.push(m[1])
    s = t.slice(c + 1)
  }
  return mods
}

/** 自 z.object 顶层起按名字读取一个 `z.name(...)` 调用的参数文本（需以 z. 开头）。 */
function callArg(s) {
  const t = s.trim()
  const m = t.match(/^z\.([A-Za-z]+)\s*\(/)
  if (!m) return null
  const openIdx = m[0].length - 1
  const c = findClosing(t, openIdx)
  if (c < 0) return null
  return { name: m[1], argText: t.slice(openIdx + 1, c), closeIdx: c, text: t }
}

const PRIMITIVE_NAMES = new Set([
  'string', 'number', 'boolean', 'unknown', 'undefined', 'null', 'any',
  'void', 'date', 'bigint', 'never',
])

/** z.union 参数文本（可能以 `[` 开头）→ 各对象成员的 key 依次并入，去重保序。 */
function unionMemberFields(argText) {
  const members = splitTopLevelItems(firstBracket(argText))
  const out = []
  for (const mm of members) {
    const tm = mm.trim()
    if (!tm || /^z\.(?:undefined|void|null)\(/.test(tm)) continue
    for (const k of fieldsOf(tm)) if (!out.includes(k)) out.push(k)
  }
  return out
}

/**
 * 抽取“包络内的字段名集合”（字段名有序列表）：
 * - z.object → 顶层 key 列表
 * - z.array(z.object(...)) → 元素对象 key
 * - z.union([成员...]) → 各对象成员的 key 依次并入（跳过 undefined/void/null 成员）
 */
function fieldsOf(text) {
  const call = callArg(text)
  if (!call) return []
  if (call.name === 'object') return objectKeys(call.argText) ?? []
  if (call.name === 'array') return fieldsOf(call.argText)
  if (call.name === 'union') return unionMemberFields(call.argText)
  return []
}

/**
 * 对一条 `const ...$schema = z.xxx(...)` 的表达式文本生成结构摘要与字段列表。
 * 只处理包络层：对象列 key、union/enum/intersection 计成员数、array/record/literal/lazy
 * 只标形状、原语用名字，尾部修饰链（optional/nullable/readonly/default...）以 `.名` 级联。
 */
function summarizeExpr(expr) {
  const t = expr.trim()
  const call = callArg(t)
  if (!call) return { summary: 'unknown', fields: [] }
  const { name, argText } = call
  const mods = parseModifiers(t.slice(call.closeIdx + 1))
  const modStr = mods.length > 0 ? '.' + mods.join('.') : ''

  let base
  let fields = []
  if (name === 'object') {
    const keys = objectKeys(argText)
    base = keys === null ? 'object' : `object{${keys.join(',')}}`
    fields = keys ?? []
  } else if (name === 'array') {
    base = 'array'
    fields = fieldsOf(argText)
  } else if (name === 'union') {
    const cnt = splitTopLevelItems(firstBracket(argText)).filter((x) => x !== '').length
    base = `union[${cnt}]`
    fields = unionMemberFields(argText)
  } else if (name === 'intersection') {
    base = `intersection[${splitTopLevelItems(argText).filter((x) => x !== '').length}]`
  } else if (name === 'record') {
    base = 'record'
  } else if (name === 'literal') {
    base = 'literal'
  } else if (name === 'lazy') {
    base = 'lazy'
  } else if (name === 'enum') {
    base = `enum[${splitTopLevelItems(firstBracket(argText)).filter((x) => x !== '').length}]`
  } else if (PRIMITIVE_NAMES.has(name)) {
    base = name
  } else {
    base = `z.${name}`
  }
  return { summary: base + modStr, fields }
}

// ---------------------------------------------------------------------------
// 变量名 → 条目
// ---------------------------------------------------------------------------

/** 包名 → pkg key：`@deepseek-ai/dsh-api-session-controller` → `deepseek_ai_dsh_api_session_controller`。 */
export function pkgKeyFromName(pkgName) {
  return pkgName.replace('@', '').replaceAll('/', '_').replaceAll('-', '_')
}

/** 变量名去掉 `$schema`（及可能的重复序号后缀 `$schema2` 等）得到“核名”。 */
function codecCoreName(name) {
  return name.replace(/\$schema\d*$/, '')
}

/**
 * 名称是否形如远程条目（核名 `_<pkg>_..._parameter_N` 或 `_<pkg>_..._result`）。
 * 注意完整变量名以 `$schema` 结尾（如 `_..._session_attachment_result$schema`）。
 */
function isEntryLikeName(name) {
  const core = codecCoreName(name)
  return core.startsWith('_') && (/_parameter_\d+$/.test(core) || /_result$/.test(core))
}

/**
 * 用已知 pkgKey 解析条目变量名，返回 { namespace, method, kind, index } 或 null。
 * namespace = pkg 段后的第一个 `_` 段；method = 其后到 `_parameter_N`/`_result` 前的其余段。
 */
function parseEntryName(name, pkgKey) {
  const core = codecCoreName(name)
  const prefix = `_${pkgKey}_`
  if (!core.startsWith(prefix)) return null
  const suffix = core.slice(prefix.length)
  let m = suffix.match(/^(.*)_parameter_(\d+)$/)
  if (m) {
    const parts = m[1].split('_')
    if (parts.length < 2) return null
    return {
      namespace: parts[0],
      method: parts.slice(1).join('_'),
      kind: 'parameter',
      index: Number(m[2]),
    }
  }
  m = suffix.match(/^(.*)_result$/)
  if (m) {
    const parts = m[1].split('_')
    if (parts.length < 2) return null
    return { namespace: parts[0], method: parts.slice(1).join('_'), kind: 'result', index: undefined }
  }
  return null
}

/** 无 package.json 时从条目变量名集合推导 pkg key（最长公共前缀，截到完整 `_` 段）。 */
function derivePkgFromNames(names) {
  if (names.length < 2) return null
  let p = names[0]
  for (let i = 1; i < names.length; i++) {
    while (names[i].slice(0, p.length) !== p) p = p.slice(0, -1)
    if (!p) return null
  }
  const cut = p.lastIndexOf('_')
  if (cut <= 0) return null
  const pkg = p.slice(1, cut) // 去掉前导 '_' 与尾随 '_'
  return pkg !== '' ? pkg : null
}

/** 从某目录向上找最近的、带 name 的 package.json，返回 { name, version } 或 null。 */
export function findNearestPackageInfo(startDir) {
  let dir = path.resolve(startDir)
  for (;;) {
    const pj = path.join(dir, 'package.json')
    if (fs.existsSync(pj)) {
      try {
        const data = JSON.parse(fs.readFileSync(pj, 'utf8'))
        if (data && typeof data.name === 'string' && data.name !== '') {
          return { name: data.name, version: typeof data.version === 'string' ? data.version : undefined }
        }
      } catch {
        // package.json 不可解析：继续向上找
      }
    }
    const parent = path.dirname(dir)
    if (parent === dir) return null
    dir = parent
  }
}

// ---------------------------------------------------------------------------
// 解析一个 bundle 文本/文件
// ---------------------------------------------------------------------------

/**
 * 解析一份 bundle 文本，返回 { pkgKey, entries }。
 * entries 条目结构：{ namespace, method, kind, index?, fields, summary, path }。
 * - pkgKey 优先级：opts.pkgKey > 文件就近 package.json.name > 变量名公共前缀推导。
 * - 任一条目变量名解析失败、括号不平衡等 → 抛错（调用方按解析错退出非零）。
 */
export function parseBundleText(text, opts = {}) {
  const where = opts.filePath ? ` (${opts.filePath})` : ' (inline)'
  const decls = splitDeclarations(text)

  let pkgKey = opts.pkgKey ?? null
  const entryNames = decls.filter((d) => d.name && isEntryLikeName(d.name)).map((d) => d.name)
  if (entryNames.length === 0) {
    throw new Error(`解析到零条远程 schema（未找到 _<pkg>_<ns>_<method>_parameter_N|result 声明）${where}`)
  }
  if (pkgKey === null && opts.filePath) {
    const info = findNearestPackageInfo(path.dirname(opts.filePath))
    if (info && info.name) pkgKey = pkgKeyFromName(info.name)
  }
  if (pkgKey === null) pkgKey = derivePkgFromNames(entryNames)
  if (pkgKey === null) {
    throw new Error(
      `无法确定 bundle 的 pkg 前缀：请把文件放回其包目录（含 package.json）或让变量名可被公共前缀推导${where}`,
    )
  }

  const entries = []
  for (const d of decls) {
    if (!d.name || !d.name.startsWith('_')) continue // 命名 codec / 顶层辅助常量
    if (!isEntryLikeName(d.name)) continue
    const parsed = parseEntryName(d.name, pkgKey)
    if (!parsed) {
      throw new Error(`无法解析条目变量名（与 pkg 前缀 ${pkgKey} 不匹配？）: ${d.name}${where}`)
    }
    const { summary, fields } = summarizeExpr(d.expr)
    if (summary === 'unknown' || summary === 'unbalanced') {
      throw new Error(`无法解析条目表达式: ${d.name}${where}`)
    }
    const entry = {
      namespace: parsed.namespace,
      method: parsed.method,
      kind: parsed.kind,
      fields,
      summary,
      path: entryPath(parsed.namespace, parsed.method, parsed.kind, parsed.index),
    }
    if (parsed.index !== undefined) entry.index = parsed.index
    entries.push(entry)
  }
  return { pkgKey, entries }
}

/** 解析单个 bundle 文件。 */
export function parseBundleFile(filePath) {
  let text
  try {
    text = fs.readFileSync(filePath, 'utf8')
  } catch (e) {
    throw new Error(`无法读取 bundle 文件 ${filePath}: ${e.message}`)
  }
  return parseBundleText(text, { filePath })
}

// ---------------------------------------------------------------------------
// 侧（side）加载 / 聚合 / 版本
// ---------------------------------------------------------------------------

/** 条目稳定标识：`ns.method:param#N` 或 `ns.method:result`。 */
export function entryPath(namespace, method, kind, index) {
  return kind === 'result' ? `${namespace}.${method}:result` : `${namespace}.${method}:param#${index}`
}

/**
 * 递归收集目录下所有 basename 以 `typert.remote-client.js` 结尾的文件。
 * npm 包树里的包目录可能是符号链接（本机 profiles/node_modules/@deepseek-ai 即如此），
 * 因此跟随目录/文件符号链接，但用“真实路径去重”防止符号链接环造成死循环。
 */
function walkBundleFiles(dir, out, seen) {
  let real
  try {
    real = fs.realpathSync(dir)
  } catch {
    return // 无法解析的链接目录：跳过
  }
  if (seen.has(real)) return
  seen.add(real)

  let entries
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true })
  } catch (e) {
    throw new Error(`无法读取目录 ${dir}: ${e.message}`)
  }
  for (const ent of entries) {
    const full = path.join(dir, ent.name)
    let isDir = ent.isDirectory()
    let isFile = ent.isFile()
    if (ent.isSymbolicLink()) {
      try {
        const st = fs.statSync(full) // 跟随链接再判断
        isDir = st.isDirectory()
        isFile = st.isFile()
      } catch {
        continue // 悬空链接：跳过
      }
    }
    if (isDir) walkBundleFiles(full, out, seen)
    else if (isFile && BUNDLE_NAME_RE.test(ent.name)) out.push(full)
  }
}

/** 解析 CLI 侧输入（单文件或目录）→ 有序文件列表；不存在/无匹配 → 抛错。 */
export function collectBundleFiles(input) {
  let st
  try {
    st = fs.statSync(input)
  } catch {
    throw new Error(`输入路径不存在: ${input}`)
  }
  if (st.isFile()) {
    if (!BUNDLE_NAME_RE.test(path.basename(input))) {
      throw new Error(`输入文件不是 typert.remote-client.js: ${input}`)
    }
    return [path.resolve(input)]
  }
  if (st.isDirectory()) {
    const out = []
    walkBundleFiles(input, out, new Set())
    if (out.length === 0) {
      throw new Error(`目录中未找到任何 typert.remote-client.js 文件: ${input}`)
    }
    return out.map((p) => path.resolve(p)).sort()
  }
  throw new Error(`输入既不是文件也不是目录: ${input}`)
}

/**
 * 版本解析：hint（--*-version）> 输入是包目录读其 package.json > 首个 bundle
 * 就近 package.json 的 version > "unknown"。
 */
function resolveVersion(input, files, hint) {
  if (hint !== undefined && hint !== null && hint !== '') return String(hint)
  try {
    const st = fs.statSync(input)
    if (st.isDirectory()) {
      const pj = path.join(input, 'package.json')
      if (fs.existsSync(pj)) {
        try {
          const data = JSON.parse(fs.readFileSync(pj, 'utf8'))
          if (data && typeof data.version === 'string' && data.version !== '') return data.version
        } catch { /* 忽略坏 package.json，继续回退 */ }
      }
    }
  } catch { /* 路径在 collect 阶段已校验过，忽略 */ }
  if (files.length > 0) {
    const info = findNearestPackageInfo(path.dirname(files[0]))
    if (info && info.version !== undefined) return info.version
  }
  return 'unknown'
}

/**
 * 加载一侧输入 → { input, files, entries, version }。
 * 同一 path 出现多次且摘要一致 → 去重；摘要冲突 → 抛错（视为脏数据）。
 */
export function loadSide(input, versionHint) {
  const files = collectBundleFiles(input)
  const byPath = new Map()
  for (const f of files) {
    const { entries } = parseBundleFile(f)
    for (const e of entries) {
      const prev = byPath.get(e.path)
      if (prev !== undefined) {
        if (prev.summary !== e.summary || !sameFields(prev.fields, e.fields)) {
          throw new Error(`同一侧出现同 path 但结构冲突的重复条目: ${e.path}`)
        }
        continue // 完全一致 → 去重
      }
      byPath.set(e.path, e)
    }
  }
  if (byPath.size === 0) {
    throw new Error(`解析到零条远程 schema（${files.length} 个 bundle 文件，均无 _<pkg>_<ns>_<method>_parameter|result 条目）`)
  }
  const entries = [...byPath.values()]
  const version = resolveVersion(input, files, versionHint)
  return { input, files, entries, version }
}

// ---------------------------------------------------------------------------
// schema 快照（snapshot）：规范 JSON 导出 / 反序列化回 side
// ---------------------------------------------------------------------------
//
// 快照 JSON 形如（schema_version 固定 1；对象 key 全部字典序，namespace/method 名
// 排序，parameter 按 index 排序 → 同一输入两次运行字节一致）：
// ```json
// {
//   "namespaces": {
//     "<ns>": {
//       "methods": {
//         "<method>": {
//           "parameter": [
//             { "fields": ["a", "b"], "index": 0, "summary": "object{a,b}" }
//           ],
//           "result": { "fields": ["c"], "summary": "string.optional" }
//         }
//       }
//     }
//   },
//   "schema_version": 1,
//   "version": "0.1.2-rc.1"
// }
// ```
// 叶子数据与 loadSide/parseBundleText 产出的条目一一对应（fields/summary 原样保存），
// 因此 flattenNamespaceTree 可无损重建 `{ entries, version }`——快照-vs-快照 与
// bundle-vs-bundle 走同一条 diffSides，结果一致。

/**
 * 把扁平条目列表组装成规范 namespaces 树（namespace/method 字典序、parameter 按 index
 * 排序；叶子/结构对象的 key 均按字典序书写，输出天然可复现）。
 */
export function buildNamespaceTree(entries) {
  const nsMap = new Map() // ns → Map(method → Map(kind → data))
  for (const e of entries) {
    let methodMap = nsMap.get(e.namespace)
    if (!methodMap) {
      methodMap = new Map()
      nsMap.set(e.namespace, methodMap)
    }
    let kindMap = methodMap.get(e.method)
    if (!kindMap) {
      kindMap = new Map()
      methodMap.set(e.method, kindMap)
    }
    if (e.kind === 'parameter') {
      let arr = kindMap.get('parameter')
      if (!arr) {
        arr = []
        kindMap.set('parameter', arr)
      }
      arr.push({ fields: e.fields, index: e.index, summary: e.summary })
    } else {
      kindMap.set('result', { fields: e.fields, summary: e.summary })
    }
  }
  const out = {}
  for (const ns of [...nsMap.keys()].sort()) {
    const methodsOut = {}
    const methodMap = nsMap.get(ns)
    for (const method of [...methodMap.keys()].sort()) {
      const kindMap = methodMap.get(method)
      const mOut = {}
      const params = kindMap.get('parameter')
      if (params) {
        mOut.parameter = [...params].sort((a, b) => a.index - b.index)
      }
      const result = kindMap.get('result')
      if (result) mOut.result = result
      methodsOut[method] = mOut
    }
    out[ns] = { methods: methodsOut }
  }
  return out
}

/** 由一侧输入（loadSide/loadSnapshotFile 返回的 side）构造快照对象。 */
export function makeSnapshot(side) {
  return {
    namespaces: buildNamespaceTree(side.entries),
    schema_version: 1,
    version: side.version ?? 'unknown',
  }
}

/** 把快照 JSON 的 namespaces 树展开回扁平条目列表（带结构校验），非法 → 抛错。 */
export function flattenNamespaceTree(namespaces, where = '') {
  const ctx = where ? `（${where}）` : ''
  if (!namespaces || typeof namespaces !== 'object' || Array.isArray(namespaces)) {
    throw new Error(`快照缺少 namespaces 对象${ctx}`)
  }
  const entries = []
  const push = (e) => {
    e.path = entryPath(e.namespace, e.method, e.kind, e.index)
    entries.push(e)
  }
  for (const [nsName, nsVal] of Object.entries(namespaces)) {
    if (
      !nsVal || typeof nsVal !== 'object' || Array.isArray(nsVal) ||
      !nsVal.methods || typeof nsVal.methods !== 'object' || Array.isArray(nsVal.methods)
    ) {
      throw new Error(`快照 namespace "${nsName}" 结构非法（需 { methods: {...} }）${ctx}`)
    }
    for (const [methodName, mVal] of Object.entries(nsVal.methods)) {
      if (!mVal || typeof mVal !== 'object' || Array.isArray(mVal)) {
        throw new Error(`快照 method "${nsName}.${methodName}" 结构非法${ctx}`)
      }
      const params = mVal.parameter === undefined ? [] : mVal.parameter
      if (!Array.isArray(params)) {
        throw new Error(`快照 method "${nsName}.${methodName}" 的 parameter 必须是数组${ctx}`)
      }
      for (const [i, p] of params.entries()) {
        const ok =
          p && typeof p === 'object' && !Array.isArray(p) &&
          Number.isInteger(p.index) && p.index >= 0 &&
          Array.isArray(p.fields) && p.fields.every((f) => typeof f === 'string') &&
          typeof p.summary === 'string'
        if (!ok) {
          throw new Error(
            `快照 method "${nsName}.${methodName}" 第 ${i} 个 parameter 条目结构非法（需 { index, fields, summary }）${ctx}`,
          )
        }
        push({ namespace: nsName, method: methodName, kind: 'parameter', index: p.index, fields: p.fields, summary: p.summary })
      }
      if (mVal.result !== undefined) {
        const r = mVal.result
        const ok =
          r && typeof r === 'object' && !Array.isArray(r) &&
          Array.isArray(r.fields) && r.fields.every((f) => typeof f === 'string') &&
          typeof r.summary === 'string'
        if (!ok) {
          throw new Error(
            `快照 method "${nsName}.${methodName}" 的 result 条目结构非法（需 { fields, summary }）${ctx}`,
          )
        }
        push({ namespace: nsName, method: methodName, kind: 'result', fields: r.fields, summary: r.summary })
      }
    }
  }
  return entries
}

/**
 * 加载快照 JSON 文件 → side（{ input, files, entries, version }），供 diff 使用。
 * 校验：schema_version===1、非空 version、namespaces 结构、无重复 path、零条报错。
 */
export function loadSnapshotFile(filePath) {
  let raw
  try {
    raw = fs.readFileSync(filePath, 'utf8')
  } catch (e) {
    throw new Error(`无法读取快照文件 ${filePath}: ${e.message}`)
  }
  let snap
  try {
    snap = JSON.parse(raw)
  } catch (e) {
    throw new Error(`快照文件不是合法 JSON: ${filePath}（${e.message}）`)
  }
  if (!snap || typeof snap !== 'object' || Array.isArray(snap)) {
    throw new Error(`快照文件顶层必须是 JSON 对象: ${filePath}`)
  }
  if (snap.schema_version !== 1) {
    throw new Error(`快照 schema_version 不是 1（本工具仅支持 schema_version:1，got ${String(snap.schema_version)}）: ${filePath}`)
  }
  if (typeof snap.version !== 'string' || snap.version === '') {
    throw new Error(`快照缺少非空 version 字符串: ${filePath}`)
  }
  const entries = flattenNamespaceTree(snap.namespaces, filePath)
  if (entries.length === 0) {
    throw new Error(`快照中无任何条目: ${filePath}`)
  }
  const seen = new Set()
  for (const e of entries) {
    if (seen.has(e.path)) throw new Error(`快照出现重复 path: ${e.path}（${filePath}）`)
    seen.add(e.path)
  }
  return { input: filePath, files: [filePath], entries, version: snap.version }
}

// ---------------------------------------------------------------------------
// diff / 变更描述 / 输出
// ---------------------------------------------------------------------------

/** 两个有序字段列表是否逐项相同。 */
function sameFields(a, b) {
  return a.length === b.length && a.every((v, i) => v === b[i])
}

/** 生成 changed 条目的中文变更摘要（对象/元素字段增删、union 成员数变化等）。 */
function makeChangeText(a, b) {
  const parts = []
  const removed = a.fields.filter((k) => !b.fields.includes(k))
  const added = b.fields.filter((k) => !a.fields.includes(k))
  if (removed.length > 0) parts.push(`移除字段: ${removed.join(',')}`)
  if (added.length > 0) parts.push(`新增字段: ${added.join(',')}`)
  const am = a.summary.match(/^union\[(\d+)\]/)
  const bm = b.summary.match(/^union\[(\d+)\]/)
  if (am && bm && am[1] !== bm[1]) parts.push(`union 成员数 ${am[1]} → ${bm[1]}`)
  if (parts.length === 0) parts.push('结构摘要变化')
  return parts.join('；')
}

/** 序列化单条目为输出对象（added/removed 用）。 */
function toAddRemoveItem(e) {
  const item = {
    namespace: e.namespace,
    method: e.method,
    kind: e.kind,
    ...(e.index !== undefined ? { index: e.index } : {}),
    path: e.path,
    fields: e.fields,
    summary: e.summary,
  }
  return item
}

/**
 * 两侧 diff → SchemaDiff 对象。
 * fromSide/toSide = loadSide 的返回（或测试用 { entries, version }）。
 */
export function diffSides(fromSide, toSide) {
  const fromMap = new Map(fromSide.entries.map((e) => [e.path, e]))
  const toMap = new Map(toSide.entries.map((e) => [e.path, e]))

  const added = []
  const removed = []
  const changed = []
  for (const [p, e] of toMap) {
    if (!fromMap.has(p)) added.push(toAddRemoveItem(e))
  }
  for (const [p, e] of fromMap) {
    if (!toMap.has(p)) removed.push(toAddRemoveItem(e))
  }
  for (const [p, e] of toMap) {
    const f = fromMap.get(p)
    if (f && (f.summary !== e.summary || !sameFields(f.fields, e.fields))) {
      changed.push({
        namespace: e.namespace,
        method: e.method,
        kind: e.kind,
        ...(e.index !== undefined ? { index: e.index } : {}),
        path: p,
        from: f.summary,
        to: e.summary,
        change: makeChangeText(f, e),
      })
    }
  }
  const byPath = (x, y) => (x.path < y.path ? -1 : x.path > y.path ? 1 : 0)
  added.sort(byPath)
  removed.sort(byPath)
  changed.sort(byPath)

  return {
    schema_version: 1,
    from_version: fromSide.version ?? 'unknown',
    to_version: toSide.version ?? 'unknown',
    added,
    removed,
    changed,
  }
}

// ---------------------------------------------------------------------------
// 原子写（参照项目 config::atomic_write_0600 的同目录 .tmp + rename 惯例）
// ---------------------------------------------------------------------------

/** 原子写 JSON：同目录 `<path>.tmp` 写好后 rename；失败清理 tmp 并抛错。 */
export function writeJsonAtomic(filePath, obj) {
  const dir = path.dirname(path.resolve(filePath))
  const tmp = path.join(dir, `${path.basename(filePath)}.tmp`)
  const data = `${JSON.stringify(obj, null, 2)}\n`
  fs.writeFileSync(tmp, data, { encoding: 'utf8', mode: 0o644 })
  try {
    fs.renameSync(tmp, path.resolve(filePath))
  } catch (e) {
    try { fs.unlinkSync(tmp) } catch { /* 尽力清理 */ }
    throw e
  }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const USAGE = `用法:
  # diff 模式（缺省）：对比两个 bundle，或两个 schema 快照（两侧可混用 bundle/快照）
  node scripts/schema-compare.mjs --from <dir-or-file> --to <dir-or-file> \\
       [--from-version X] [--to-version Y] [--out path]
  node scripts/schema-compare.mjs --from-snapshot <file> --to-snapshot <file> [--out path]

  # snapshot 模式：把一份 bundle 导出为规范 schema 快照 JSON（golden 入库）
  node scripts/schema-compare.mjs --mode snapshot --bundle <dir-or-file> --version X \\
       --out schemas/dsh-api-schema-X.json

对比两个版本的 typert.remote-client.js 包树/单文件（或已导出的 schema 快照），抽取
namespace/method 结构树并输出 SchemaDiff JSON（schema_version:1，含
added/removed/changed）。diff 输出到 stdout（缺省）或 --out 指定的文件（原子写）；
snapshot 输出为规范快照 JSON（确定格式，必填 --out 原子写）。

选项（diff 模式）:
  --from <dir|file>       from 侧输入：单个 typert.remote-client.js 或含此类文件的目录
  --to <dir|file>         to 侧输入（同上）
  --from-snapshot <file>  from 侧输入：schema 快照 JSON（版本内嵌于文件）
  --to-snapshot <file>    to 侧输入：schema 快照 JSON
  --from-version X        覆盖 from bundle 版本号（缺省读 package.json，取不到为 "unknown"）
  --to-version Y          覆盖 to bundle 版本号（仅 bundle 侧；快照版本内嵌于文件）
  --out path              输出文件（原子写：同目录 .tmp + rename）；缺省打印到 stdout

选项（snapshot 模式）:
  --bundle <dir|file>     要导出的 bundle 输入（收集规则同 diff 的 --from/--to）
  --version X             快照版本号（必填）
  --out path              输出文件（原子写：同目录 .tmp + rename，必填）

通用:
  --mode <diff|snapshot>  模式：diff（缺省）或 snapshot
  -h, --help              显示本帮助

SchemaDiff JSON 形如:
  { "schema_version":1, "from_version":"...", "to_version":"...",
    "added":[{namespace,method,kind,index?,path,fields,summary}],
    "removed":[...],
    "changed":[{namespace,method,kind,index?,path,from,to,change}] }

Snapshot JSON 形如（对象 key 字典序、namespace/method 排序 → 确定性输出）:
  { "namespaces": { "<ns>": { "methods": { "<method>": {
        "parameter": [{ "fields": [...], "index": N, "summary": "..." }],
        "result":    { "fields": [...], "summary": "..." } } } } },
    "schema_version": 1, "version": "..." }

错误处理: 输入路径不存在 / 无匹配文件 / 零条 schema / bundle 无法解析 / 快照 JSON
非法（schema_version≠1、缺 version、结构坏、重复 path）→ stderr 报错且 exit code 1，
绝不产出半成品文件。`

/** 简易参数解析：--flag value 或 --flag=value；未知选项报错。 */
function parseArgs(argv) {
  const args = {
    mode: 'diff',
    bundle: null,
    version: null,
    from: null,
    to: null,
    fromVersion: null,
    toVersion: null,
    fromSnapshot: null,
    toSnapshot: null,
    out: null,
    help: false,
  }
  const known = new Set([
    '--mode', '--bundle', '--version',
    '--from', '--to', '--from-version', '--to-version',
    '--from-snapshot', '--to-snapshot', '--out', '--help', '-h',
  ])
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]
    if (a === '--help' || a === '-h') { args.help = true; continue }
    let key = a
    let val = null
    const eq = a.indexOf('=')
    if (a.startsWith('--') && eq > 0) {
      key = a.slice(0, eq)
      val = a.slice(eq + 1)
    }
    if (!known.has(key)) throw new Error(`未知参数: ${a}\n\n${USAGE}`)
    if (key !== '--help' && key !== '-h') {
      if (val === null) {
        i++
        if (i >= argv.length) throw new Error(`参数 ${key} 缺少取值\n\n${USAGE}`)
        val = argv[i]
      }
      if (key === '--mode') args.mode = val
      else if (key === '--bundle') args.bundle = val
      else if (key === '--version') args.version = val
      else if (key === '--from') args.from = val
      else if (key === '--to') args.to = val
      else if (key === '--from-version') args.fromVersion = val
      else if (key === '--to-version') args.toVersion = val
      else if (key === '--from-snapshot') args.fromSnapshot = val
      else if (key === '--to-snapshot') args.toSnapshot = val
      else if (key === '--out') args.out = val
    }
  }
  return args
}

/**
 * 每侧输入解析：bundle（--from/--to）或快照（--from-snapshot/--to-snapshot）二选一。
 * flagBase 用于报错文案（如 '--from' → 对偶快照参数 '--from-snapshot'）。
 */
function resolveDiffSide(flagBase, bundleInput, versionHint, snapFile) {
  if (bundleInput !== null && snapFile !== null) {
    throw new Error(`不能同时指定 ${flagBase} 与 ${flagBase}-snapshot\n\n${USAGE}`)
  }
  if (snapFile !== null) {
    if (versionHint !== null) {
      throw new Error(`版本参数不能用于 snapshot 侧（快照版本内嵌于文件）\n\n${USAGE}`)
    }
    return loadSnapshotFile(snapFile)
  }
  if (bundleInput === null) {
    throw new Error(`缺少必填参数 ${flagBase}\n\n${USAGE}`)
  }
  return loadSide(bundleInput, versionHint)
}

/** diff 模式主流程：两侧各取 bundle 或快照 → 同一 diffSides。 */
function runDiffCli(args) {
  if (args.bundle !== null || args.version !== null) {
    throw new Error(`diff 模式不接受 --bundle/--version（属 snapshot 模式）\n\n${USAGE}`)
  }
  const fromSide = resolveDiffSide('--from', args.from, args.fromVersion, args.fromSnapshot)
  const toSide = resolveDiffSide('--to', args.to, args.toVersion, args.toSnapshot)
  const diff = diffSides(fromSide, toSide)
  if (args.out !== null) {
    writeJsonAtomic(args.out, diff)
  } else {
    console.log(`${JSON.stringify(diff, null, 2)}\n`)
  }
  return 0
}

/** snapshot 模式主流程：解析 bundle → 规范快照 JSON → 原子写（--out 必填）。 */
function runSnapshotCli(args) {
  const diffFlags = [
    ['--from', args.from], ['--to', args.to],
    ['--from-snapshot', args.fromSnapshot], ['--to-snapshot', args.toSnapshot],
    ['--from-version', args.fromVersion], ['--to-version', args.toVersion],
  ]
  const bad = diffFlags.filter(([, v]) => v !== null).map(([k]) => k)
  if (bad.length > 0) {
    throw new Error(`snapshot 模式不接受 ${bad.join('/')}（diff 参数）\n\n${USAGE}`)
  }
  if (args.bundle === null) {
    throw new Error(`snapshot 模式缺少必填参数 --bundle\n\n${USAGE}`)
  }
  if (args.version === null || args.version === '') {
    throw new Error(`snapshot 模式缺少必填参数 --version\n\n${USAGE}`)
  }
  if (args.out === null || args.out === '') {
    throw new Error(`snapshot 模式缺少必填参数 --out\n\n${USAGE}`)
  }
  const side = loadSide(args.bundle, args.version)
  const snap = makeSnapshot(side)
  writeJsonAtomic(args.out, snap)
  return 0
}

/** CLI 主流程（同步）。错误一律：stderr 明确报错 + exit code 1，不产半成品。 */
export function runCli(argv) {
  try {
    const args = parseArgs(argv)
    if (args.help) {
      console.log(USAGE)
      return 0
    }
    if (args.mode === 'snapshot') return runSnapshotCli(args)
    if (args.mode === 'diff') return runDiffCli(args)
    throw new Error(`--mode 只支持 diff|snapshot（got "${args.mode}"）\n\n${USAGE}`)
  } catch (e) {
    process.stderr.write(`[schema-compare] error: ${e.message}\n`)
    return 1
  }
}

// 直接执行时才跑 CLI（被 schema-compare.test.mjs import 时不触发）
const isMain = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (isMain) {
  const code = runCli(process.argv.slice(2))
  process.exitCode = code
}
