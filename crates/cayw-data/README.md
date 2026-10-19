# cayw-data

由现有 CAYW Host 的权限与审计入口调用的纯数据能力。输入内容显式放在参数中；本 crate 不读取文件、启动进程、访问网络、注册 Agent 工具或创建权限链。不要把这些能力绕过宿主直接挂到 Agent。

## 公共接口

```rust
pub fn definitions() -> Vec<cayw_ai::ToolDefinition>;

pub fn execute(
    name: &str,
    args: serde_json::Value,
    cancel: cayw_ai::CancellationToken,
    limits: cayw_data::Limits,
) -> impl std::future::Future<
    Output = Result<serde_json::Value, cayw_codemode::CapabilityError>,
> + Send;
```

仍通过 `execute(...).await` 取得结果。调用时立即接管参数的清理责任，返回的 future 即使从未运行就被丢弃，也会逐层拆开深层 JSON 后释放，不递归释放整棵树；开始运行 future 后才启动数据工作。

唯一规范名字和输入/输出如下。所有顶层参数对象、二进制输入对象和变换分支都拒绝额外字段。执行前先校验完整参数的 JSON Schema；数值字节、行下标和 context 还必须使用整数 token，`1.0`、`"1"` 都不能代替整数。

| 名字 | 参数示例 | 结果示例 |
| --- | --- | --- |
| `data.parse` | `{"format":"json","text":"{\"id\":\"001\"}"}` | `{"value":{"id":"001"}}` |
| `data.serialize` | `{"format":"json","value":{"id":"001"}}` | `{"text":"{\"id\":\"001\"}"}` |
| `data.query` | `{"value":{"a/b":{"~x":["001"]}},"pointer":"/a~1b/~0x/0"}` | `{"value":"001"}` |
| `data.transform` | `{"value":{"a":1,"b":"2"},"operation":"select_fields","fields":["b"]}` | `{"value":{"b":"2"}}` |
| `data.transform` | `{"value":["a","b","c"],"operation":"select_rows","indices":[2,0]}` | `{"value":["c","a"]}` |
| `text.diff` | `{"before":"a\n","after":"b\n","context":0}` | `{"diff":"--- before\n+++ after\n@@ -1,1 +1,1 @@\n-a\n+b\n"}` |
| `hash.digest` | `{"algorithm":"sha256","input":{"kind":"utf8","value":"abc"}}` | `{"hex":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}` |
| `codec.encode` | `{"encoding":"base64","input":{"kind":"bytes","value":[0,255,128]}}` | `{"text":"AP+A"}` |
| `codec.decode` | `{"encoding":"hex","text":"e99baa","output":"utf8"}` | `{"text":"雪"}` |

`data.transform` 可选 `pointer`，默认空字符串表示根。返回选择后的对象或数组，不修改输入中的其他位置。字段必须存在；行下标从 **0** 开始，按请求顺序输出，禁止重复选择。JSON Pointer 支持 RFC 6901 的 `~0` / `~1`；数组下标拒绝 `00`、`-`、负数及不存在的下标。

## 格式边界

`format` 为 `json` / `toml` / `yaml` / `xml` / `csv`，输入都是 UTF-8 字符串。序列化输出为紧凑、确定的规范形式，不保留源文件排版。

- **JSON**：拒绝重复键、尾随内容、非法转义、非有限数以及溢出的整数。整数保留 i64/u64 范围，绝不把超范围整数悄悄转成浮点数。带小数点或指数的数字使用有限 f64 的表示精度；如需十进制任意精度，调用方必须传字符串。
- **TOML**：文档根必须是对象。支持字符串、布尔、i64 整数、有限浮点数、数组和表；拒绝 null、超 i64 的整数、原生日期时间、NaN/Inf。需要日期字符串时在原 TOML 中明确加引号。输出使用带引号的键、内联表和数组，不把 JSON 空值或大整数变成别的类型。输入先做保守的深度/词法数量检查，再调用保留默认递归保护的 TOML 解析器，最后检查实际结果。
- **YAML**：支持单文档、块/流数组和映射、引号/块字符串，以及 JSON 数字语法、YAML 1.2 null/布尔值。映射键必须为字符串，重复键和未加引号的合并键 `<<` 报错；带引号的 `"<<"` / `'<<'` 是普通字符串键，可在任意支持的嵌套位置读写。拒绝所有锚点、别名及显式标签，因此不会展开别名；拒绝非有限数。看起来像数字但不符合 JSON 数字语法的普通标量（如 `01`、`0x10`、`+1`、日期）须明确加引号，不能悄悄降成字符串。输出为 JSON 流式语法，它是 YAML 1.2 的子集，每个字符串/键都会加引号。
- **XML**：使用下述显式 XML 1.0 节点模型，保留子元素、混合文本顺序和字符串属性，不声称与任意 JSON 无损互转。相邻文本/CDATA/引用合并成一个文本节点，CDATA 的字面拼写不保留。空元素与显式开闭元素等价。文档根前后的 XML 空白作为排版忽略。拒绝 DTD、外部/自定义实体、重复属性、非法 XML 名字/字符、多根文档；节点模型不承载声明、注释和处理指令，遇到这些内容会报 `unsupported`，不静默丢弃。属性名和元素名按 XML 1.0 Name 处理；带冒号的名字是原样名字，不做 namespace URI 解析。实体仅支持五个 XML 内置实体和合法数字字符引用。序列化保留 CR 和属性空白的字符引用，避免读回时发生换行归一化损失。
- **CSV**：固定逗号分隔、双引号转义、LF/CRLF 行尾，第一行是唯一 header。值为 `{"header":["id","note"],"rows":[["001","雪"]]}`。所有单元格始终为字符串，禁止自动数字/布尔/null 转换。保留列顺序、嵌入换行和空字段；拒绝重复列名、行宽不一致、未闭合/散落引号、引号后的非法内容及会被宽松解析器跳过的空行。单列空字段应明确写为 `""`。输出所有字段均带引号，因此首列表头中的 U+FEFF 是字段内容，读回不会被当作文件 BOM 删除。读取外部 CSV 时，文件最开头的 BOM 按文件标记处理，可位于带引号的表头之前；只去除这一处文件标记，字段中的字面 U+FEFF 应写在引号内。

XML 示例：

```json
{
  "name": "根",
  "attributes": {"id": "a&\"b"},
  "children": [
    {"text": "前&"},
    {"name": "b", "attributes": {}, "children": [{"text": "雪<"}]},
    {"text": "后"}
  ]
}
```

XML 元素对象必须且只能包含 `name`、`attributes`、`children`。文本节点必须且只能包含一个字符串字段 `text`。序列化不会把未声明字段忽略掉。

## 二进制、编码、哈希与 diff

`hash.digest` / `codec.encode` 的 `input` 必须显式选一种表示：

```json
{"kind":"utf8","value":"雪"}
{"kind":"bytes","value":[0,255,128]}
{"kind":"base64","value":"AP+A"}
```

Base64 使用标准字母表和规范 padding，无需 padding 时不添加 `=`；需要 padding 时不能省略。拒绝空白、URL-safe 字母表、不规范尾位和中间 padding。hex 接受大小写数字，输出小写。`codec.decode` 的 `output` 只能是 `bytes` 或 `utf8`；非法 UTF-8 返回 `encoding`，绝不使用替换字符。字节数组返回 `{"bytes":[...]}`；UTF-8 返回 `{"text":"..."}`。

哈希算法为 `sha256` / `blake3`，结果是 32 字节小写 hex。没有任意长度输出接口。

Diff 按保留行尾的行计算，`context` 默认 3、允许 0..100。固定标签 `before` / `after`；行范围统一带 count；相同内容返回空 diff。保留原 CRLF，缺少末尾换行时输出标准 `\\ No newline at end of file` 标记。不会为巨量行分配无界矩阵：计算前检查矩阵单元数，比较中继续累计字节工作量，超限报错。

## 资源与取消

`Limits::default()`：

| 字段 | 默认值 | 硬上限 |
| --- | ---: | ---: |
| `input_bytes` | 1 MiB | 16 MiB |
| `output_bytes` | 1 MiB | 16 MiB |
| `max_depth` | 32 | 64 |
| `max_nodes` | 100000 | 1000000 |
| `max_rows` | 10000 | 1000000 |
| `max_diff_work` | 1000000 | 16000000 |

所有值必须非零。字节数按**整个 JSON 参数/结果对象**计算，包含字段名、引号和转义，不只是 text 的长度。深度从逻辑数据根的 1 开始，节点数包含对象键；数据封装另有固定小额结构额度，不能借给逻辑数据。`codec.encode` / `hash.digest` 的字节数组输入以及 `codec.decode` 的字节数组输出都包含数组根节点：空数组占 1 个节点，单字节数组占 2 个节点、深度为 2。UTF-8 / Base64 字符串按 1 个字符串节点计数，字节处理量仍受完整参数和结果的字节上限约束。每个数组、CSV 表、XML 子列表和 diff 的每侧分别检查 `max_rows`。XML 节点模型的实际对象/数组层级也计入结果深度。Diff 工作量为矩阵单元数加比较字节数；16M 工作单元的硬上限同时限制矩阵最多约 64 MB，实际字符比较还会减少可用单元。

解析逐项检查节点/行/深度，写出时限制缓冲区，最终返回前再次检查结果封装。CPU 工作运行在 Tokio 的 blocking worker，执行器仍能投递取消；循环、写入与解析阶段边界检查令牌。取消不是强杀线程：单次第三方字符串扫描/TOML 解析调用最多处理 `input_bytes` 的已受限输入，会在下一检查点退出。Drop 执行 future 会取消其子令牌。全部工作没有外部副作用，所有错误 `outcome_unknown` 为 false。

不提供无限并发或全进程总内存限制；生产 Host 应在现有 gate 中限制同时执行的调用数量。这里限制单次调用的数据和工作量。

## 错误

错误使用现有 `CapabilityError`，`message` 带稳定类别前缀，例如：

```text
arguments: ...               # Schema 不符、错误的模型结构或整数 token
syntax: ...                  # 坏语法、重复键、非法引用等
unsupported: ...             # 明确不支持且无法保留的数据类型/内容
selection: ...               # pointer、字段或行不存在
encoding: ...                # 解码后不是合法 UTF-8
input_limit: ...
output_limit: ...
depth_limit: ...
node_limit: ...
row_limit: ...
diff_work_limit: ...
invalid_limits: ...
cancelled: ...
unknown_capability: ...
```

Schema 不符只返回固定大小的 `arguments` 消息，不把完整输入复制进错误；选择列表去重使用有序集合，避免 Schema 的复杂项成对比较。错误细节用于诊断，不承诺整段文字固定。输入过大或深度不合法时会先拒绝，再进入 Schema 校验，避免验证器遍历无界输入；所有实际能力均在 Schema 校验后分派。

## 真实 Luau 调用与复跑

[examples/data.luau](examples/data.luau) 在真实 Luau VM 内依次 parse → query → select rows → serialize → sha256。[tests/luau.rs](tests/luau.rs) 仅通过现有 `CapabilityHost` 组合数据分派，断言结果和宿主审计调用顺序，没有另一条授权链。生产 Host 把 `definitions()` 纳入唯一 catalog，并在已有 gate 授权和审计后调用 `execute()`。

仓库根目录下运行，仅检查本 crate：

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 fmt -p cayw-data -- --check
/Users/chengwen/.cargo/bin/cargo +1.95.0 check -p cayw-data --locked --offline
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-data --locked --offline
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-data --all-targets --locked --offline -- -D warnings
```

测试覆盖各格式往返、转义/Unicode、重复键/属性、坏语法、数字溢出、类型保留、输入/输出/行/节点/深度/diff 工作量限制、执行中取消、公开哈希向量和真实 Luau 宿主调用。额外回归覆盖 CSV 的 BOM 表头、YAML 普通 `"<<"` 键与合并/别名拒绝、二进制数组根节点预算；深达 100000 层的参数分别在独立子进程内验证从未运行就丢弃、取消分支先选中、运行后报深度超限三种情况，并检查子进程退出状态。离线测试不调用 Provider。平台验收以实际运行结果为准；本地 macOS 测试不能替代 Windows/Linux 验收。

## 依赖与许可证

版本和许可经 `cargo info` 核实；仓库 Cargo.lock 由 Cargo 自动解析。直接依赖：

| 包 | Manifest 版本 | 上游许可 |
| --- | --- | --- |
| `quick-xml` | 0.42.0 | MIT |
| `yaml-rust2`（关闭 encoding feature） | 0.13.0 | MIT OR Apache-2.0 |
| `toml` | 1.1.8 | MIT OR Apache-2.0 |
| `csv` | 1.4.0 | Unlicense/MIT（选 MIT） |
| `base64` | 0.22.1 | MIT OR Apache-2.0 |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 |
| `blake3` | 1.8.7 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception（选 Apache-2.0） |

其余使用本仓库的 cayw-ai/cayw-codemode 与现有 serde/serde_json/tokio/async-trait/jsonschema（默认关闭外部引用解析功能；仅编译本 crate 的内置 Schema）。上述直接依赖的 MIT/Apache-2.0 选项兼容本项目 Apache-2.0；新增依赖均通过正常 Cargo 依赖引入，未复制第三方源码。
