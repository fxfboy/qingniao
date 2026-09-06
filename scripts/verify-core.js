// 青鸟核心逻辑验证脚本（Node 22+）
import assert from 'node:assert';
import cryptoNode from 'node:crypto';

// ===== 1. 签名算法验证：与飞书官方 Python 实现对照 =====
async function computeSign(timestamp, secret) {
  const keyData = new TextEncoder().encode(timestamp + '\n' + secret);
  const key = await crypto.subtle.importKey('raw', keyData, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const sig = await crypto.subtle.sign('HMAC', key, new Uint8Array(0));
  return btoa(String.fromCharCode(...new Uint8Array(sig)));
}

// 飞书官方 Python 对照实现
function pythonImpl(timestamp, secret) {
  const stringToSign = `${timestamp}\n${secret}`;
  const hmac = cryptoNode.createHmac('sha256', stringToSign).digest();
  return hmac.toString('base64');
}

(async () => {
  // 已知测试向量（飞书文档示例: timestamp=1700000000, secret=test-secret）
  const cases = [
    ['1700000000', 'test-secret'],
    ['1754290000', 'abc123'],
    ['1700000000', ''],
  ];
  for (const [ts, secret] of cases) {
    const a = await computeSign(ts, secret);
    const b = pythonImpl(ts, secret);
    assert.strictEqual(a, b, `sign mismatch for ${ts}/${secret}: ${a} vs ${b}`);
    console.log(`✓ 签名一致: ts=${ts} sign=${a.slice(0, 12)}…`);
  }

  // ===== 2. 富文本行解析验证 =====
  function splitInner(inner) {
    const i = inner.indexOf('](');
    if (i === -1) return [inner, ''];
    return [inner.slice(0, i), inner.slice(i + 2)];
  }
  function parsePostLine(line) {
    const nodes = [];
    const re = /(!\[[^\]]*\]\([^)]*\)|\[[^\]]*\]\([^)]*\))/g;
    let last = 0, m;
    while ((m = re.exec(line)) !== null) {
      if (m.index > last) nodes.push({ tag: 'text', text: line.slice(last, m.index) });
      const token = m[0];
      if (token.startsWith('!')) {
        const [alt, key] = splitInner(token.slice(2, -1));
        nodes.push({ tag: 'img', image_key: key.trim() });
      } else {
        const [text, href] = splitInner(token.slice(1, -1));
        nodes.push({ tag: 'a', text, href: href.trim() });
      }
      last = m.index + token.length;
    }
    if (last < line.length) nodes.push({ tag: 'text', text: line.slice(last) });
    return nodes;
  }

  const t1 = parsePostLine('季度报告已发布');
  assert.deepStrictEqual(t1, [{ tag: 'text', text: '季度报告已发布' }]);
  console.log('✓ 纯文本行解析');

  const t2 = parsePostLine('查看报告[点击这里](https://feishu.cn/docx/abc)谢谢');
  assert.deepStrictEqual(t2, [
    { tag: 'text', text: '查看报告' },
    { tag: 'a', text: '点击这里', href: 'https://feishu.cn/docx/abc' },
    { tag: 'text', text: '谢谢' },
  ]);
  console.log('✓ 行内链接解析');

  const t3 = parsePostLine('![架构图](img_ecffc3b9)');
  assert.deepStrictEqual(t3, [{ tag: 'img', image_key: 'img_ecffc3b9' }]);
  console.log('✓ 行内图片解析');

  const t4 = parsePostLine('看[图](https://a.cn/x)和![图2](img_abc)吧');
  assert.deepStrictEqual(t4, [
    { tag: 'text', text: '看' },
    { tag: 'a', text: '图', href: 'https://a.cn/x' },
    { tag: 'text', text: '和' },
    { tag: 'img', image_key: 'img_abc' },
    { tag: 'text', text: '吧' },
  ]);
  console.log('✓ 混合解析');

  // ===== 3. 消息 payload 组装验证 =====
  function buildPostPayload(title, lines) {
    const content = lines.filter((l) => l.trim() !== '').map(parsePostLine);
    return {
      msg_type: 'post',
      content: { post: { zh_cn: { title: title || undefined, content } } },
    };
  }
  const payload = buildPostPayload('季度报告', ['报告已发布', '[下载](https://x.com/f.pdf)']);
  assert.strictEqual(payload.msg_type, 'post');
  assert.strictEqual(payload.content.post.zh_cn.title, '季度报告');
  assert.strictEqual(payload.content.post.zh_cn.content.length, 2);
  assert.strictEqual(payload.content.post.zh_cn.content[1][0].tag, 'a');
  // 空标题应省略 title 字段
  const p2 = buildPostPayload('', ['你好']);
  assert.strictEqual(JSON.stringify(p2).includes('"title"'), false);
  console.log('✓ payload 组装与空标题省略');

  console.log('\n全部验证通过 ✅');
})().catch((e) => { console.error('验证失败:', e.message); process.exit(1); });
