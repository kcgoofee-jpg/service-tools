/**
 * 首页用到的接口数据形状（Rust 网关返回）。
 * 字段一律可缺省：网关只会多给、不会改名；缺哪个字段就不显示哪块，不编数字。只列页面真正读的字段。
 */

/** /public/live 里的「我的排队」：登录成员才有 */
export type MineItem = { state: "running" } | { state: "waiting"; position: number; eta?: number };
export type LiveMine = { mine?: MineItem[]; waiting?: number; running?: number };

export type Blocked = { code?: string; text?: string };

export type Live = {
  quota?: {
    images_today?: number;
    images_cap?: number;
    v5_today?: number;
    v5_cap?: number;
  };
  queue?: { waiting?: number; running?: number };
  guard?: {
    hour?: number;
    hour_cap?: number;
    cooldown?: number;
    blocked?: Blocked | null;
  };
  rules?: {
    queue_cap?: number;
    hourly_cap?: number;
    quiet_start?: number;
    quiet_end?: number;
    quiet_cap?: number;
  };
  /** 一张图的平均放行间隔（秒）：排队时间估算用 */
  interval?: number;
  me?: LiveMine | null;
} & Meta;

/** 变化很慢的站点信息：/public/status 与 /public/live 都带（live 里是 30 秒子缓存） */
export type Meta = {
  discord_invite?: string;
  has_announcement?: boolean;
  algo_notice?: string;
  audit_notice?: string;
  web_login?: boolean;
  /** 加入按钮跟着它走：注册关了就置灰成「关闭注册」；role_note = 限定身份组的名称（只有设了门槛才有） */
  registration?: { open?: boolean; role_note?: string };
  limits?: {
    key_image_queue?: number;
    quiet_start?: number;
    quiet_end?: number;
    quiet?: boolean;
  };
};

export type Coupon = { id: number; kind?: string; note?: string; expires_at: number };

export type Discord = {
  id?: string;
  username?: string | null;
  display_name?: string | null;
  avatar?: string | null;
};

export type MeGuest = { logged_in: false };
export type MeMember = {
  logged_in: true;
  discord?: Discord | null;
  has_key: boolean;
  key?: string;
  expires_at?: number | null;
  image_scope?: string;
  today?: {
    images?: number;
    v5?: number;
    legacy?: number;
    daily_images?: number;
    daily_v5?: number;
    anlas?: number;
    daily_anlas?: number;
    reset_hour?: number | null;
  };
  coupons?: Coupon[];
  /** 我的平均出图耗时：只用来补状态卡那句「平均约 N 秒」 */
  timing?: { avg_dur_s?: number | null } | null;
};
export type Me = MeGuest | MeMember;
