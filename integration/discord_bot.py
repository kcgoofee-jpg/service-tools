"""Standalone Discord bot: exposes /register in one server and hands off to the gateway."""
from __future__ import annotations

import os

import discord
from discord import app_commands

from discord_registration import handle_register


def main() -> None:
    guild = discord.Object(id=int(os.environ["DISCORD_GUILD_ID"]))
    client = discord.Client(intents=discord.Intents.none())
    tree = app_commands.CommandTree(client)

    @tree.command(name="register", description="领取你的 NAI Gate API Key", guild=guild)
    async def register(interaction: discord.Interaction):
        await handle_register(interaction)

    @client.event
    async def on_ready():
        await tree.sync(guild=guild)
        print(f"[bot] ready as {client.user}, /register synced")

    client.run(os.environ["DISCORD_BOT_TOKEN"])


if __name__ == "__main__":
    main()
