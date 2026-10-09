"""Standalone Discord bot: /register /quota /resetkey /help for members, /slots /revoke for admins."""
from __future__ import annotations

import os
import time

import discord
import httpx
from discord import app_commands

from discord_registration import handle_register

BACKEND = os.getenv("REGISTRATION_BACKEND_URL", "http://127.0.0.1:3003").rstrip("/")
SITE = os.getenv("SITE_URL", "").rstrip("/")


async def backend(path: str, interaction: discord.Interaction, extra_id: str | None = None):
    """POST to the gateway bridge. Returns (status, json-or-detail-text)."""
    payload = {"discord_id": extra_id or str(interaction.user.id), "guild_id": str(interaction.guild_id)}
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

    @tree.command(name="register", description="领取你的 NAI Gate API Key", guild=guild)
    async def register(interaction: discord.Interaction):
        await handle_register(interaction)

    @tree.command(name="quota", description="查看今天还剩多少额度、Key 何时过期", guild=guild)
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
        if data["expires_at"]:
            days = max(0, int((data["expires_at"] - time.time()) // 86400))
            lines.append(f"Key 剩余有效期：约 {days} 天")
        if not data["enabled"]:
            lines.append("⚠ 这把 Key 已被停用，请联系站长。")
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

    @tree.command(name="help", description="怎么使用 NAI Gate", guild=guild)
    async def help_(interaction: discord.Interaction):
        text = ("1. 用 `/register` 领取 Key，会私信发给你。\n"
                f"2. 在柏宝绘等支持自定义 NovelAI 地址的客户端里，接口地址填 `{SITE}`，Key 填你领到的 `nai-…`。\n"
                "3. `/quota` 查看今日额度，`/resetkey` 重置 Key。\n"
                "4. 请勿分享 Key；额度用完次日重置。")
        await interaction.response.send_message(text, ephemeral=True)

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
        await interaction.followup.send(f"已领取 {data['active']} / {cap}{reset}", ephemeral=True)

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
