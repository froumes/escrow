# Frikadellen BAF
Frikadellen BAF in the newest minecraft version based on Rust

#if you get banned well you know you risked it its a macro in very early access
also send me logs of bans on [Discord](https://discord.gg/42DvX6T9jh) thanks


## Features

- **Automated Auction House Flips**: Monitors and executes profitable BIN (Buy It Now) auctions
- **Bazaar Trading**: Automated bazaar order management and flipping
- **Microsoft Authentication**: Secure login with your Microsoft/Minecraft account
- **Hypixel Integration**: Direct connection to Hypixel Skyblock servers
- **Real-time Updates**: WebSocket connection to Coflnet for flip notifications
- **Configurable**: Easy-to-use configuration system

# Installation on a Linux VPS:
wget https://github.com/TreXito/frikadellen-baf-121/releases/latest/download/frikadellen_baf-linux-x86_64 && chmod +x frikadellen_baf-linux-x86_64
then you can run it everytime with ./frikadellen_baf-linux-x86_64

OR the loader:

wget https://github.com/TreXito/frikadellen-baf-121/releases/latest/download/FrikadellenBAF-loader-linux-x86_64  && chmod +x FrikadellenBAF-loader-linux-x86_64


## Quick Start

1. Download the latest release for your platform from the [Releases](../../releases) page
2. Run the executable
3. Enter your Minecraft username when prompted
4. Complete Microsoft authentication in the browser that opens
5. The bot will connect to Hypixel and start monitoring for flips

Follow the prompts shown in the terminal and browser for Microsoft authentication setup.

## Configuration

The application creates a `config.toml` file in the same directory as the executable. You can manually edit this file to customize settings:

- `ingame_name`: Your Minecraft username
- `enable_ah_flips`: Enable/disable auction house flips
- `enable_bazaar_flips`: Enable/disable bazaar flips
- `web_gui_port`: Port for the web interface (default: 8080)
- `home_island_command`: Command used to return "home" after joining SkyBlock or recovery checks
- `home_island_scoreboard_text`: Scoreboard text that confirms the bot is at that home location

For a friend's island, you can set:

```toml
home_island_command = "/visit FriendName"
home_island_scoreboard_text = "FriendName's Island"
```

## Requirements

- Minecraft: Java Edition license linked to a Microsoft account
- Access to Hypixel server (not banned)
- Internet connection

## Troubleshooting

If authentication fails, rerun the app and complete the Microsoft login flow again in the opened browser window.

## Building from Source

Requires Rust nightly toolchain:

```bash
rustup install nightly
rustup default nightly
cargo build --release
```

### Using the Launcher Script

For convenience, you can use the `frikadellen-baf-121` launcher script:

```bash
chmod +x frikadellen-baf-121
./frikadellen-baf-121
```

The launcher script will:
- Check for an existing binary
- Automatically build from source if needed
- Run the application with any arguments you provide

## License
AGPLv3
