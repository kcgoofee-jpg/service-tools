"""Standalone Discord bot.

成员：/register /quota /resetkey /help。管理员：/open /limit /slots /ban /unban /revoke。
功能授权和生成记录开关只在网页后台操作。
"""
from __future__ import annotations

import os
import time

import discord
import httpx
from discord import app_commands

from discord_registration import handle_register

BACKEND = os.getenv("REGISTRATION_BACKEND_URL", "http://127.0.0.1:3003").rstrip("/")
SITE = os.getenv("SITE_URL", "").rstrip("/")


STATUS_TEXT = {"ok": "🟢 正常", "idle": "🟢 正常（近期无请求）", "degraded": "🟠 不稳定（近期失败较多）"}


async def backend(path: str, interaction: discord.Interaction, extra_id: str | None = None,
                  extra: dict | None = None):
    """POST to the gateway bridge. Returns (status, json-or-detail-text)."""
    payload = {"discord_id": extra_id or str(interaction.user.id), "guild_id": str(interaction.guild_id),
               "actor_id": str(interaction.user.id), **(extra or {})}
    try:
        async with httpx.AsyncClient(timeout=8) as client:
            response = await client.post(BACKEND + path, json=payload,
                                         headers={"Authorization": "Bearer " + os.environ["REGISTRATION_BRIDGE_SECRET"]})
        data = response.json()
    except (httpx.HTTPError, ValueError):
        return 0, "服务暂时不可用，请稍后再试。"
    return response.status_code, data if response.status_code == 200 else data.get("detail", "请求失败。")


def build_client() -> tuple[discord.Client, app_commands.CommandTree, discord.Object]:
    guild = discord.Object(id=int(os.environ["DISCORD_GUILD_ID"]))
    client = discord.Client(intents=discord.Intents.none())
    tree = app_commands.CommandTree(client)

    @tree.command(name="register", description="领取你的猫头鹰公益站 API Key", guild=guild)
    async def register(interaction: discord.Interaction):
        await handle_register(interaction)

    @tree.command(name="quota", description="查看今日额度、Key 有效期和服务状态", guild=guild)
    async def quota(interaction: discord.Interaction):
        await interaction.response.defer(ephemeral=True)
        status, data = await backend("/self-register/quota", interaction)
        if status != 200:
            await interaction.followup.send(str(data), ephemeral=True)
            return
        lines = [f"今日图片：{data['images']} / {data['daily_images']}"]
        if data["daily_v5"]:
            lines.append(f"今日 V5：{data['v5']} / {data['daily_v5']}")
        scope = "含 V5" if data["image_model_scope"] == "all" else "仅 V4.5 及以下"
        lines.append(f"可用模型：{scope}")
        lines.append("已开通功能：" + ("、".join(f["label"] for f in data["features"] if f["on"]) or "无"))
        closed = [f["label"] for f in data["features"] if not f["on"]]
        if closed:
            lines.append("未开通：" + "、".join(closed))
        if data["expires_at"]:
            days = max(0, int((data["expires_at"] - time.time()) // 86400))
            lines.append(f"Key 剩余有效期：约 {days} 天")
        if not data["enabled"]:
            lines.append("⚠ 这把 Key 已被停用，请联系站长。")
        code, info = await backend("/self-register/info", interaction)
        if code == 200:
            up = info["upstream"]
            state = f"服务状态：{STATUS_TEXT.get(up['status'], up['status'])}"
            if up["recent"]:
                state += f"（近 10 分钟 {up['recent']} 次请求，失败 {up['failed']} 次）"
            lines.append(state)
            if up["image_cooldown_seconds"]:
                lines.append(f"⏳ 上游限流冷却中，约 {up['image_cooldown_seconds']} 秒后恢复生图")
        await interaction.followup.send("\n".join(lines), ephemeral=True)

    @tree.command(name="resetkey", description="Key 丢了或泄露了？换一把新的（旧的立即失效）", guild=guild)
    async def resetkey(interaction: discord.Interaction):
        await interaction.response.defer(ephemeral=True)
        status, data = await backend("/self-register/resetkey", interaction)
        if status != 200:
            await interaction.followup.send(str(data), ephemeral=True)
            return
        await interaction.followup.send(f"新的 Key（只有你能看到这条消息，请妥善保存）：\n`{data['key']}`\n旧 Key 已失效。",
                                        ephemeral=True)

    @tree.command(name="help", description="怎么使用猫头鹰公益站", guild=guild)
    async def help_(interaction: discord.Interaction):
        await interaction.response.defer(ephemeral=True)
        status, data = await backend("/self-register/info", interaction)
        lines = [f"`/register` 领取 Key（私信发送，含配置教程）· `/quota` 额度和服务状态 · `/resetkey` 换新 Key",
                 f"客户端接口地址填 `{SITE}`（不加 /v1），Key 填 `nai-` 开头的整串。详细说明：{SITE}"]
        if status == 200:
            if not data["open"]:
                lines.append("⚠ 目前暂未开放注册。")
            if data["notice"]:
                lines.append("📢 " + data["notice"])
        await interaction.followup.send("\n".join(lines), ephemeral=True)

    async def run_ops(interaction: discord.Interaction, action: str, **extra):
        await interaction.response.defer(ephemeral=True)
        code, data = await backend("/self-register/ops", interaction, extra={"action": action, **extra})
        await interaction.followup.send(data["message"] if code == 200 else str(data), ephemeral=True)

    @tree.command(name="open", description="（管理员）开放或关闭自助注册", guild=guild)
    @app_commands.default_permissions(manage_guild=True)
    @app_commands.choices(state=[app_commands.Choice(name="开放", value="on"), app_commands.Choice(name="关闭", value="off")])
    async def open_(interaction: discord.Interaction, state: app_commands.Choice[str]):
        await run_ops(interaction, "open", value=state.value)

    @tree.command(name="limit", description="（管理员）设置名额上限，0 为不限", guild=guild)
    @app_commands.default_permissions(manage_guild=True)
    async def limit(interaction: discord.Interaction, count: app_commands.Range[int, 0, 1000]):
        await run_ops(interaction, "limit", value=str(count))

    @tree.command(name="ban", description="（管理员）永久禁止某位成员领取 Key，并撤销其现有 Key", guild=guild)
    @app_commands.default_permissions(manage_guild=True)
    async def ban(interaction: discord.Interaction, member: discord.Member):
        await run_ops(interaction, "ban", target=str(member.id))

    @tree.command(name="unban", description="（管理员）解除某位成员的领取禁令", guild=guild)
    @app_commands.default_permissions(manage_guild=True)
    async def unban(interaction: discord.Interaction, member: discord.Member):
        await run_ops(interaction, "unban", target=str(member.id))

    @tree.command(name="slots", description="（管理员）查看名额占用", guild=guild)
    @app_commands.default_permissions(manage_guild=True)
    async def slots(interaction: discord.Interaction):
        await interaction.response.defer(ephemeral=True)
        status, data = await backend("/self-register/slots", interaction)
        if status != 200:
            await interaction.followup.send(str(data), ephemeral=True)
            return
        cap = data["max"] or "不限"
        reset = f"，每天 {data['reset_at']} 自动清空" if data["reset_at"] else ""
        state = "开放中" if data.get("open", True) else "已关闭"
        await interaction.followup.send(f"已领取 {data['active']} / {cap}{reset}；注册{state}", ephemeral=True)

    @tree.command(name="revoke", description="（管理员）撤销某位成员的 Key，释放名额", guild=guild)
    @app_commands.default_permissions(manage_guild=True)
    async def revoke(interaction: discord.Interaction, member: discord.Member):
        await interaction.response.defer(ephemeral=True)
        status, data = await backend("/self-register/revoke", interaction, extra_id=str(member.id))
        await interaction.followup.send(f"已撤销 {member.mention} 的 Key。" if status == 200 else str(data),
                                        ephemeral=True)

    @client.event
    async def on_ready():
        await tree.sync(guild=guild)
        print(f"[bot] ready as {client.user}, commands synced", flush=True)

    return client, tree, guild


def main() -> None:
    client, _tree, _guild = build_client()
    client.run(os.environ["DISCORD_BOT_TOKEN"])


if __name__ == "__main__":
    main()
