import type { ComponentChildren } from "preact";
import { Card, CopyField, Tabs } from "../../ui";
import { useStore } from "./ctx";
import { StatusCard } from "./StatusCard";

/* ---------------- 加入 Discord ---------------- */

/** 邀请链接只认 https；自助领 Key 关闭（registration.open=false）时置灰成「关闭注册」；没配链接就给一句说明，别留空卡片。 */
export function JoinDiscord(props: {
  variant?: "primary" | "default";
  size?: "md" | "lg";
  fallback?: boolean;
}) {
  const { meta } = useStore();
  const m = meta.value;
  const cls = `btn${props.variant === "primary" ? " btn--primary" : ""}${props.size === "lg" ? " btn--lg" : ""}`;
  if (m.registration?.open === false) {
    return (
      <button type="button" class={cls} disabled title="注册已关闭，开放时间看站长公告">
        关闭注册
      </button>
    );
  }
  const invite = m.discord_invite ?? "";
  if (/^https:\/\//i.test(invite)) {
    return (
      <a class={cls} href={invite} target="_blank" rel="noopener noreferrer">
        加入 Discord
      </a>
    );
  }
  return props.fallback ? <p class="muted small">用站长公告里的邀请链接加入服务器。</p> : null;
}

/* ---------------- 首屏（游客） ---------------- */

export function GuestHero() {
  const { meta } = useStore();
  const login = meta.value.web_login;
  // 首屏不放按钮：登录在右上角，加入 Discord 在下面第 1 步——同一个动作整页只出现一次
  return (
    <section class="lp-hero" aria-labelledby="lp-title">
      <div class="lp-hero__copy">
        <h1 id="lp-title">猫头鹰公益站</h1>
        <p class="lp-lead">Discord 社区成员共享的 NovelAI 公益中转，用你常用的客户端出图。</p>
        <p class="muted small">
          {login === true
            ? "新成员按下面三步开始；已经领过 Key 的，点右上角「登录」查看自己的额度。"
            : "新成员按下面三步开始；领到 Key 后直接填进客户端即可使用。"}
        </p>
      </div>
      <StatusCard />
    </section>
  );
}

/* ---------------- 三步上手 ---------------- */

export function Steps() {
  const { meta } = useStore();
  const closed = meta.value.registration?.open === false;
  const site = location.origin;
  return (
    <section aria-labelledby="lp-steps">
      <h2 id="lp-steps" class="lp-h2">
        三步上手
      </h2>
      <ol class="lp-steps">
        <li class="card lp-step">
          <h3>
            <span class="lp-n" aria-hidden="true">
              1
            </span>
            加入 Discord
          </h3>
          <p class="muted small">
            <span class="lp-nb">先加入猫头鹰公益站的 Discord 服务器，</span>
            <span class="lp-nb">Key 和公告都在那里：</span>
          </p>
          <div class="lp-step__act">
            <JoinDiscord variant="primary" fallback />
          </div>
        </li>
        <li class="card lp-step">
          <h3>
            <span class="lp-n" aria-hidden="true">
              2
            </span>
            领取你的 Key
          </h3>
          <p class="muted small">
            <span class="lp-nb">在猫头鹰公益站的 Discord 服务器里，</span>
            <span class="lp-nb">任意频道输入：</span>
          </p>
          <div class="lp-step__act">
            <CopyField value="/register" label="领取命令" />
          </div>
          <p class="muted small lp-step__note">
            <span class="lp-nb">要在本站的服务器里发送，</span>
            <span class="lp-nb">别的服务器或私聊里不会生效。</span>
          </p>
          {closed && <p class="muted small">注册当前已关闭，开放时间看站长公告。</p>}
        </li>
        <li class="card lp-step">
          <h3>
            <span class="lp-n" aria-hidden="true">
              3
            </span>
            填入客户端
          </h3>
          <p class="muted small">接口地址填这个，API Key 填你的 Key：</p>
          <div class="lp-step__act">
            <CopyField value={site} label="接入地址" />
          </div>
        </li>
      </ol>
    </section>
  );
}

/* ---------------- 客户端设置 ---------------- */

function Guides() {
  const { facts } = useStore();
  const interval = facts.value.interval;
  const ms = interval > 0 ? Math.round(interval * 1000) : 0;
  return (
    <Card title="客户端设置">
      <Tabs
        label="客户端"
        tabs={[
          {
            id: "baibai",
            label: "柏宝绘",
            panel: () => (
              <div class="stack">
                <ol class="lp-list">
                  <li>打开「渠道」→「配置」</li>
                  <li>点击「新建接入点」</li>
                  <li>「接口地址」填写上面步骤 3 的地址</li>
                  <li>「API Key」填写你的 Key</li>
                </ol>
                <p class="muted small">任何支持自定义 NovelAI 地址的客户端都可以按同样方式填写。</p>
              </div>
            ),
          },
          {
            id: "chatu8",
            label: "智绘姬",
            panel: () => (
              <div class="stack">
                <p class="muted small">SillyTavern 生图插件 st-chatu8。</p>
                <ol class="lp-list">
                  <li>
                    「站点」选<b>其他站点</b>，接口地址填<b>完整地址</b>（接口地址后拼{" "}
                    <code>/ai/generate-image</code>，带路径，<b>不要只填根地址</b>
                    ）——只填根地址的话，遇到限流插件会多发一个无效请求。
                  </li>
                  <li>
                    「生图间隔时间」从默认 100ms 调大
                    {ms ? (
                      <>
                        到 <b>{ms} 以上</b>（毫秒）——本站每 {interval} 秒放行一张，默认值会连续撞限流。
                      </>
                    ) : (
                      <>——本站出图有最小间隔，默认值会连续撞限流。</>
                    )}
                  </li>
                  <li>
                    一条回复尽量只放 1~2 个 image 标签；「自动点击生成」「永久自动点击」保持关闭（默认已关）。
                  </li>
                  <li>
                    多人共用的高峰时段建议<b>开启「启用云端队列」</b>
                    （默认队列地址即可）——排队由插件的队列服务承担，生图请求会被排好队再发到站里。
                  </li>
                  <li>
                    遇到 429 表示本时段份额用完，<b>按提示里的等待时间再点</b>
                    （插件不会自动重试，也不会自己多发请求）；402 /「需要有效订阅」是额度问题；403 是 Key
                    被暂停，提示里会写原因。
                  </li>
                </ol>
              </div>
            ),
          },
        ]}
      />
    </Card>
  );
}

/* ---------------- 使用须知 ---------------- */

/** 括号里的补充说明不拆开：窄屏换行只发生在句子之间，不会把「（30 天后自动删除）」折成两行 */
function KeepParens(props: { text: string }) {
  return (
    <>
      {props.text
        .split(/(（[^）]*）)/)
        .filter(Boolean)
        .map((s, i) =>
          s.startsWith("（") ? (
            <span key={`${i}-${s}`} class="lp-nb">
              {s}
            </span>
          ) : (
            s
          ),
        )}
    </>
  );
}

function Rule(props: { k: string; children: ComponentChildren }) {
  return (
    <div class="lp-rule">
      <dt>{props.k}</dt>
      <dd>{props.children}</dd>
    </div>
  );
}

function Rules() {
  const { meta, facts } = useStore();
  const m = meta.value;
  const f = facts.value;
  const extra = m.limits?.key_image_queue ?? 0;
  const quiet = m.limits?.quiet === true;
  const qs = m.limits?.quiet_start ?? f.quietStart;
  const qe = m.limits?.quiet_end ?? f.quietEnd;
  const audit = (m.audit_notice ?? "").trim();
  return (
    <Card title="使用须知">
      <dl class="lp-rules">
        <Rule k="规格">
          推荐 <code>V4.5 Full</code> 或 <code>V5 Full</code>。免费尺寸不超过
          1024×1024（或等像素），步数不超过 28。Vibe 要消耗 Anlas，每人每天有上限。
        </Rule>
        <Rule k="额度">
          <KeepParens text="V4.5 和 V5 都有每人每日上限，全站共用、先到先得，每日 7 点重置。全站每小时上限按最近 60 分钟（滚动窗口）计算，不是整点重置。" />
        </Rule>
        <Rule k="出图节奏">
          每把 Key 同时只能生成 1 张{extra > 0 ? `，最多再排 ${extra} 张` : ""}
          。被拒后请不要连点重试：短时间内被拒太多次，Key 会被暂时暂停。
        </Rule>
        {quiet && qs != null && qe != null && (
          <Rule k="安静时段">
            每天 {qs}:00–{qe}:00 {f.quietCap === 0 ? `暂停出图，${qe} 点恢复` : "出图放慢"}。
          </Rule>
        )}
        <Rule k="常用命令">
          在猫头鹰公益站的 <span class="lp-nb">Discord 服务器</span>里输入 <code>/quota</code> 查额度，
          <code>/resetkey</code> 换新 Key。
        </Rule>
        <Rule k="一人一把">
          Key 仅限本人使用，共用或转售会被回收；退出 <span class="lp-nb">Discord 服务器</span>后 Key
          也会被回收。
        </Rule>
        <Rule k="合规">禁止违法或极端有害的内容，违规者永久封禁。</Rule>
        {audit && (
          <Rule k="记录说明">
            <KeepParens text={audit} />
          </Rule>
        )}
      </dl>
    </Card>
  );
}

export function GuestGuides() {
  return (
    <>
      <Guides />
      <Rules />
    </>
  );
}
