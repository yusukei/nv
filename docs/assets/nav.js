/*
 * サイトの目次とフッターの前後のリンク(唯一の定義元)。
 * ページを足す・名前を変えるときはここだけを直す。名前は、そのページの h1 と <title> の前半と一字一句そろえる。
 * 各 HTML には次の 2 つだけを置く:
 *   <nav id="sitenav" class="sidebar" data-root="<docs への相対>" data-page="<docs から見たこのページ>"></nav>
 *   <div class="footer" id="pager"></div>
 *
 * 目次はどのページでも同じ型にする。違うのは光る項目だけ。
 *   仕様書(入口へ) → 道具の切り替え → 要求仕様 / 詳細仕様の切り替え → いまの冊のまとまり
 * 冊のまとまりは、要求仕様は「全体 → 要求 → 確認」、詳細仕様は「全体 → 設計 → 参考」の順。
 * 「参考」は中身のある冊にだけ出す(いまは wrangler-compose の詳細仕様だけ)。
 * 入口(docs/index.html)はどの冊にも属さないので、まとまりの代わりに 4 冊の入口を「冊」として並べる。
 * フッターの前後のリンクは、入口 → 4 冊を BOOK_ORDER の順に 1 本につなぐ。
 */
(function () {
  "use strict";

  var TOOLS = [
    { key: "wrangler-compose", label: "wrangler-compose" },
    { key: "nv", label: "nv" }
  ];
  var KINDS = [
    { key: "requirements", label: "要求仕様" },
    { key: "detail", label: "詳細仕様" }
  ];

  var BOOKS = {
    "wrangler-compose/requirements": [
      { label: "全体", items: [
          ["index.html", "全体像"]
        ] },
      { label: "要求", items: [
          ["write.html", "01. compose ファイルを書く"],
          ["commands.html", "02. コマンド"],
          ["common.html", "03. 全体の条件と範囲"]
        ] },
      { label: "確認", items: [
          ["basis.html", "決定と用語"],
          ["acceptance.html", "受け入れ条件"]
        ] }
    ],
    "wrangler-compose/detail": [
      { label: "全体", items: [
          ["index.html", "全体像"]
        ] },
      { label: "設計", items: [
          ["d01-read.html", "D01. compose ファイルを読み、設定を作る"],
          ["d02-local.html", "D02. build と手元の up"],
          ["d03-remote.html", "D03. リモートの up"],
          ["d04-ops.html", "D04. down・ps・logs・backup・restore・dry-run"],
          ["d05-test.html", "D05. 引き継ぎ・試験・確かめていないこと"]
        ] },
      { label: "参考", items: [
          ["local-support.html", "手元で使えないもの"]
        ] }
    ],
    "nv/requirements": [
      { label: "全体", items: [
          ["index.html", "全体像"]
        ] },
      { label: "要求", items: [
          ["node.html", "01. Node と依存"],
          ["tool.html", "02. 道具としての決まり"]
        ] },
      { label: "確認", items: [
          ["basis.html", "決定と用語"],
          ["acceptance.html", "受け入れ条件"]
        ] }
    ],
    "nv/detail": [
      { label: "全体", items: [
          ["index.html", "全体像"]
        ] },
      { label: "設計", items: [
          ["d01-base.html", "D01. 作りと置き場"],
          ["d02-node.html", "D02. Node の取得と実行"],
          ["d03-dist.html", "D03. スキル・配り方・更新・試験・確かめていないこと"]
        ] }
    ]
  };

  var BOOK_ORDER = ["wrangler-compose/requirements", "wrangler-compose/detail", "nv/requirements", "nv/detail"];
  var BOOK_LABEL = {
    "wrangler-compose/requirements": "wrangler-compose 要求仕様",
    "wrangler-compose/detail": "wrangler-compose 詳細仕様",
    "nv/requirements": "nv 要求仕様",
    "nv/detail": "nv 詳細仕様"
  };

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  function link(className, href, text) {
    var a = el("a", className, text);
    a.href = href;
    return a;
  }

  // 入口から 4 冊の終わりまでを、読む順に 1 本に並べたもの
  function readingOrder() {
    var out = [{ path: "index.html", label: "仕様書" }];
    BOOK_ORDER.forEach(function (book) {
      BOOKS[book].forEach(function (g) {
        g.items.forEach(function (it) {
          out.push({ path: book + "/" + it[0], label: it[1], book: BOOK_LABEL[book] });
        });
      });
    });
    return out;
  }

  function where(cur) {
    var parts = cur.split("/");
    if (parts.length < 3) return { tool: null, kind: null, book: null };
    return { tool: parts[0], kind: parts[1], book: parts[0] + "/" + parts[1] };
  }

  function renderNav(root, base, cur) {
    var w = where(cur);

    var home = link("nav-home" + (cur === "index.html" ? " active" : ""), base + "index.html");
    home.appendChild(el("span", "nh-kicker", "wrangler-compose・nv"));
    home.appendChild(el("span", "nh-title", "仕様書"));
    root.appendChild(home);

    // 幅の狭い画面では、ここから下をたたむ
    var toggle = el("button", "nav-menu", "目次");
    toggle.type = "button";
    toggle.setAttribute("aria-expanded", "false");
    root.appendChild(toggle);
    var body = el("div", "nav-body");
    root.appendChild(body);
    toggle.addEventListener("click", function () {
      var open = !root.classList.contains("open");
      root.classList.toggle("open", open);
      toggle.setAttribute("aria-expanded", String(open));
    });

    var tools = el("div", "nav-sites");
    TOOLS.forEach(function (t) {
      tools.appendChild(link("nav-site" + (t.key === w.tool ? " active" : ""),
        base + t.key + "/" + (w.kind || "requirements") + "/index.html", t.label));
    });
    body.appendChild(tools);

    var kinds = el("div", "nav-sites");
    KINDS.forEach(function (k) {
      kinds.appendChild(link("nav-site" + (k.key === w.kind ? " active" : ""),
        base + (w.tool || "wrangler-compose") + "/" + k.key + "/index.html", k.label));
    });
    body.appendChild(kinds);

    var groups = w.book ? BOOKS[w.book] : [{ label: "冊", items: BOOK_ORDER.map(function (b) {
      return [b + "/index.html", BOOK_LABEL[b], b + "/index.html"];
    }) }];
    groups.forEach(function (g) {
      var group = el("div", "nav-group expanded");
      var head = el("div", "nav-group-toggle");
      head.appendChild(el("span", "nav-group-label", g.label));
      group.appendChild(head);
      var list = el("div", "nav-list");
      g.items.forEach(function (it) {
        var path = it[2] || w.book + "/" + it[0];
        list.appendChild(link(path === cur ? "active" : null, base + path, it[1]));
      });
      group.appendChild(list);
      body.appendChild(group);
    });
  }

  // フッター: 読む順の前と次のページ。冊が変わるときは冊の名前を添える
  function renderPager(pager, base, cur) {
    var pages = readingOrder();
    var i = 0;
    pages.forEach(function (p, n) { if (p.path === cur) i = n; });
    var here = pages[i];
    var prev = i > 0 ? pages[i - 1] : null;
    var next = i + 1 < pages.length ? pages[i + 1] : null;
    function label(p) {
      return p.book && p.book !== here.book ? p.book + " ／ " + p.label : p.label;
    }
    if (prev) {
      var a = link("pager-prev", base + prev.path);
      a.appendChild(el("span", "pager-dir", "← 前"));
      a.appendChild(el("span", "pager-label", label(prev)));
      pager.appendChild(a);
    } else {
      pager.appendChild(el("span", "pager-prev"));
    }
    if (next) {
      var b = link("pager-next", base + next.path);
      b.appendChild(el("span", "pager-dir", "次 →"));
      b.appendChild(el("span", "pager-label", label(next)));
      pager.appendChild(b);
    }
  }

  document.addEventListener("DOMContentLoaded", function () {
    var root = document.getElementById("sitenav");
    if (!root) return;
    var base = root.getAttribute("data-root") || "";
    var cur = root.getAttribute("data-page") || "index.html";
    renderNav(root, base, cur);
    var pager = document.getElementById("pager");
    if (pager) renderPager(pager, base, cur);
  });
})();
