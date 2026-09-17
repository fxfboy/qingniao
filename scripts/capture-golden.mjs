// 青鸟 golden 基线捕获（M0，Agent-CLI 方案 v3 §四 D12）
//
// 对 src/message.js 的真实函数（非重写实现）跑固定输入集，产出 tests/golden/golden.json。
// Rust 侧（M1 qingniao-core）以同一输入集断言输出与该文件逐字节一致。
//
// 用法：
//   node scripts/capture-golden.mjs           # 重新生成 tests/golden/golden.json
//   node scripts/capture-golden.mjs --check   # 与已入库基线比对，不一致则退出码 1（npm test）
//
// 行为变更纪律：任何 message.js 行为变更必须先有意识地重新生成本基线并说明原因，
// 禁止「顺手更新基线」。

import assert from 'node:assert';
import fs from 'node:fs';
import path from 'node:path';
import url from 'node:url';

const GOLDEN_PATH = path.join(
  path.dirname(url.fileURLToPath(import.meta.url)), '..', 'tests', 'golden', 'golden.json');

await import(url.pathToFileURL(path.join(
  path.dirname(url.fileURLToPath(import.meta.url)), '..', 'src', 'message.js')).href);
const m = globalThis.__qnMessage;
if (!m) { console.error('加载 src/message.js 失败：globalThis.__qnMessage 不存在'); process.exit(1); }

/** 把同步调用包装为 {output} 或 {error}，两种结果都进基线 */
function run(name, input, fn) {
  try {
    return { name, input, output: fn() };
  } catch (e) {
    return { name, input, error: e.message };
  }
}
async function runAsync(name, input, fn) {
  try {
    return { name, input, output: await fn() };
  } catch (e) {
    return { name, input, error: e.message };
  }
}

// ===== hmacSign（向量来源：verify-core.js / 飞书官方文档示例）=====
const sign = [
  await runAsync('doc-example', { timestamp: '1700000000', secret: 'test-secret' },
    () => m.hmacSign('test-secret', '1700000000')),
  await runAsync('case-2', { timestamp: '1754290000', secret: 'abc123' },
    () => m.hmacSign('abc123', '1754290000')),
  await runAsync('empty-secret', { timestamp: '1700000000', secret: '' },
    () => m.hmacSign('', '1700000000')),
  await runAsync('unicode-secret', { timestamp: '1700000001', secret: '密钥🔑' },
    () => m.hmacSign('密钥🔑', '1700000001')),
];

// ===== detectType（text, chips）=====
const detect = [
  run('plain', { text: '你好，今天下午三点开会', chips: 0 }, () => m.detectType('你好，今天下午三点开会', 0)),
  run('empty', { text: '', chips: 0 }, () => m.detectType('', 0)),
  run('whitespace-only', { text: '  \n  ', chips: 0 }, () => m.detectType('  \n  ', 0)),
  run('bold', { text: '**加粗** 重点', chips: 0 }, () => m.detectType('**加粗** 重点', 0)),
  run('heading', { text: '# 标题\n正文', chips: 0 }, () => m.detectType('# 标题\n正文', 0)),
  run('link', { text: '看[链接](https://x.com)啊', chips: 0 }, () => m.detectType('看[链接](https://x.com)啊', 0)),
  run('at-all', { text: '@所有人 注意', chips: 0 }, () => m.detectType('@所有人 注意', 0)),
  run('at-user', { text: '@张三 请查收', chips: 0 }, () => m.detectType('@张三 请查收', 0)),
  run('list', { text: '- 甲\n- 乙', chips: 0 }, () => m.detectType('- 甲\n- 乙', 0)),
  run('quote', { text: '> 引用', chips: 0 }, () => m.detectType('> 引用', 0)),
  run('card-json', { text: '{"config": {"wide_screen_mode": true}}', chips: 0 }, () => m.detectType('{"config": {"wide_screen_mode": true}}', 0)),
  run('card-fence', { text: '```card\n{"header": {}}\n```', chips: 0 }, () => m.detectType('```card\n{"header": {}}\n```', 0)),
  run('chips-only', { text: '', chips: 2 }, () => m.detectType('', 2)),
  run('chips-and-text', { text: '带说明文字', chips: 1 }, () => m.detectType('带说明文字', 1)),
];

// ===== parseInline =====
const parseInline = [
  run('plain', { text: '季度报告已发布' }, () => m.parseInline('季度报告已发布')),
  run('link', { text: '查看报告[点击这里](https://feishu.cn/docx/abc)谢谢' }, () => m.parseInline('查看报告[点击这里](https://feishu.cn/docx/abc)谢谢')),
  run('img', { text: '![架构图](img_ecffc3b9)' }, () => m.parseInline('![架构图](img_ecffc3b9)')),
  run('mixed-link-img', { text: '看[图](https://a.cn/x)和![图2](img_abc)吧' }, () => m.parseInline('看[图](https://a.cn/x)和![图2](img_abc)吧')),
  run('bold', { text: '**加粗**普通' }, () => m.parseInline('**加粗**普通')),
  run('code', { text: '前面`code`后面' }, () => m.parseInline('前面`code`后面')),
  run('at-all', { text: '@所有人 请查收' }, () => m.parseInline('@所有人 请查收')),
  run('at-tag', { text: '<at user_id="ou_123">张三</at> 你好' }, () => m.parseInline('<at user_id="ou_123">张三</at> 你好')),
  run('single-star-italic', { text: '单*星*不解析' }, () => m.parseInline('单*星*不解析')),
  run('unclosed-bold', { text: '**未闭合' }, () => m.parseInline('**未闭合')),
  run('empty', { text: '' }, () => m.parseInline('')),
  run('unicode', { text: '🎉 emoji 与 **粗体** 混排' }, () => m.parseInline('🎉 emoji 与 **粗体** 混排')),
];

// ===== mdToPost =====
const post = [
  run('single-line', { md: '季度报告已发布', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('季度报告已发布', [], undefined)),
  run('title-and-link', { md: '# 季度报告\n报告已发布\n[下载](https://x.com/f.pdf)', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('# 季度报告\n报告已发布\n[下载](https://x.com/f.pdf)', [], undefined)),
  run('code-block', { md: '正文前\n\n```js\nconsole.log(1);\nconsole.log(2);\n```\n正文后', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('正文前\n\n```js\nconsole.log(1);\nconsole.log(2);\n```\n正文后', [], undefined)),
  run('unclosed-code-block', { md: '正文\n```js\ncode-line', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('正文\n```js\ncode-line', [], undefined)),
  run('quote', { md: '> 引用行\n普通行', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('> 引用行\n普通行', [], undefined)),
  run('attachments-append', { md: '带图正文', attImageKeys: ['img_a', 'img_b'], titleOverride: undefined }, () => m.mdToPost('带图正文', ['img_a', 'img_b'], undefined)),
  run('title-override', { md: '# 标题A\n内容', attImageKeys: [], titleOverride: '覆盖标题' }, () => m.mdToPost('# 标题A\n内容', [], '覆盖标题')),
  run('lists', { md: '- 列表1\n- 列表2\n1. 有序', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('- 列表1\n- 列表2\n1. 有序', [], undefined)),
  run('blank-lines', { md: '段落一\n\n\n\n段落二', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('段落一\n\n\n\n段落二', [], undefined)),
  run('empty-md', { md: '', attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('', [], undefined)),
  run('unicode-long', { md: '🎉 '.repeat(40) + '\n' + '长'.repeat(200), attImageKeys: [], titleOverride: undefined }, () => m.mdToPost('🎉 '.repeat(40) + '\n' + '长'.repeat(200), [], undefined)),
];

// ===== extractCardJson =====
const card = [
  run('raw-json', { text: '{"config":{"wide_screen_mode":true},"elements":[{"tag":"div","text":{"tag":"lark_md","content":"正文"}}]}' }, () => m.extractCardJson('{"config":{"wide_screen_mode":true},"elements":[{"tag":"div","text":{"tag":"lark_md","content":"正文"}}]}')),
  run('card-fence', { text: '```card\n{"header":{"title":{"tag":"plain_text","content":"标题"},"template":"blue"}}\n```' }, () => m.extractCardJson('```card\n{"header":{"title":{"tag":"plain_text","content":"标题"},"template":"blue"}}\n```')),
  run('plain-text-autowrap', { text: '标题行\n正文内容' }, () => m.extractCardJson('标题行\n正文内容')),
  run('bad-json-autowrap', { text: '{"bad json' }, () => m.extractCardJson('{"bad json')),
  run('title-only-autowrap', { text: '只有标题' }, () => m.extractCardJson('只有标题')),
  run('empty-text', { text: '   \n  ' }, () => m.extractCardJson('   \n  ')),
];

// ===== buildPayload =====
const payload = [
  run('text', { text: 'hello', type: 'text', imgKeys: [], title: undefined }, () => m.buildPayload('hello', 'text', [], undefined)),
  run('post', { text: '# T\nbody', type: 'post', imgKeys: [], title: undefined }, () => m.buildPayload('# T\nbody', 'post', [], undefined)),
  run('image-first-key-only', { text: 'x', type: 'image', imgKeys: ['img_k1', 'img_k2'], title: undefined }, () => m.buildPayload('x', 'image', ['img_k1', 'img_k2'], undefined)),
  run('image-no-keys', { text: 'x', type: 'image', imgKeys: [], title: undefined }, () => m.buildPayload('x', 'image', [], undefined)),
  run('interactive-json', { text: '{"config":{},"elements":[{"tag":"div"}]}', type: 'interactive', imgKeys: [], title: undefined }, () => m.buildPayload('{"config":{},"elements":[{"tag":"div"}]}', 'interactive', [], undefined)),
  run('interactive-autowrap-with-images', { text: '卡片标题\n卡片正文', type: 'interactive', imgKeys: ['img_k1', 'img_k2'], title: undefined }, () => m.buildPayload('卡片标题\n卡片正文', 'interactive', ['img_k1', 'img_k2'], undefined)),
  run('interactive-json-with-image', { text: '{"elements":[{"tag":"div"}]}', type: 'interactive', imgKeys: ['img_k9'], title: undefined }, () => m.buildPayload('{"elements":[{"tag":"div"}]}', 'interactive', ['img_k9'], undefined)),
];

const golden = {
  version: 1,
  generator: 'scripts/capture-golden.mjs',
  source: 'src/message.js',
  cases: { hmacSign: sign, detectType: detect, parseInline, mdToPost: post, extractCardJson: card, buildPayload: payload },
};

function generate() {
  return JSON.stringify(golden, null, 2) + '\n';
}

if (process.argv.includes('--check')) {
  // 以「序列化后再解析」的结果比对：JSON.stringify 会丢弃 undefined 字段，
  // 直接比内存对象会误报（与写盘内容无关的字段差异）
  const existing = JSON.parse(fs.readFileSync(GOLDEN_PATH, 'utf-8'));
  assert.deepStrictEqual(JSON.parse(generate()), existing, 'golden 基线与当前 src/message.js 行为不一致：行为变更必须显式重新捕获并在提交说明中写明原因');
  const n = Object.values(existing.cases).reduce((s, c) => s + c.length, 0);
  console.log(`golden 校验通过（${n} 个用例）✅`);
} else {
  fs.mkdirSync(path.dirname(GOLDEN_PATH), { recursive: true });
  fs.writeFileSync(GOLDEN_PATH, generate());
  const n = Object.values(golden.cases).reduce((s, c) => s + c.length, 0);
  console.log(`已生成 ${path.relative(process.cwd(), GOLDEN_PATH)}（${n} 个用例）`);
}
