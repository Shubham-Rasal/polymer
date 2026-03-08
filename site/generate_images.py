#!/usr/bin/env python3
"""
Generate excalidraw-style images for PolyBot site using Nano Banana Pro API
(Gemini 3 Pro Image / gemini-2.5-flash-image)
"""

import warnings
warnings.filterwarnings('ignore')

import os
import time
from google import genai
from google.genai import types

API_KEY = 'AIzaSyCQMunHjobBA1-1t4HmJ4XWOYhcZBobub4'
client = genai.Client(api_key=API_KEY)

# Try models in order of preference
MODELS = ['gemini-3-pro-image-preview', 'gemini-2.5-flash-image', 'gemini-2.0-flash-exp-image-generation']

EXCALIDRAW_PREFIX = (
    "Create an excalidraw-style hand-drawn illustration with these exact characteristics: "
    "rough sketchy black lines on pure white background, hand-drawn look with slightly imperfect edges, "
    "simple geometric shapes drawn as if with a marker, no gradients or shadows, no photorealistic elements, "
    "monochrome black and white only, looks like a whiteboard sketch. "
    "The subject: "
)

IMAGES = [
    {
        "filename": "hero_dashboard.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A comprehensive trading bot dashboard for Polymarket prediction markets. Shows: "
            "a main chart with price movements, a list of active prediction market positions (YES/NO), "
            "portfolio P&L metrics, order book data, and a trading activity feed. "
            "Layout has sidebar navigation, top stats bar, main chart area, and position table."
        ),
        "placeholders": [
            "https://placehold.co/1814x1002/1a1a1a/ffffff?text=PolyBot",
            "https://placehold.co/512w",  # srcset variant
        ],
        "srcset_sizes": ["1814x1002"],
    },
    {
        "filename": "market_ticker.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A live prediction market ticker display showing real-time market data. Shows: "
            "horizontal scrolling ticker with market names (ELECTIONS, CRYPTO, SPORTS), "
            "YES/NO prices, volume data, and trending market indicators. "
            "Simple table/grid layout with market categories."
        ),
        "srcset_sizes": ["1628x812"],
    },
    {
        "filename": "token_sale_card.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A token sale / presale interface card showing: "
            "a progress bar for token sale allocation, investment tiers, "
            "early access benefits list, lock-up period timeline, "
            "and a 'Buy Tokens' button. Compact card layout."
        ),
        "srcset_sizes": ["392x434"],
    },
    {
        "filename": "ai_features_card.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "An AI-powered trading features showcase card showing: "
            "neural network diagram connecting to market data, "
            "list of AI capabilities (sentiment analysis, pattern recognition, auto-trading), "
            "strategy performance metrics, and trading automation controls."
        ),
        "srcset_sizes": ["384x305"],
    },
    {
        "filename": "strategy_deploy_card.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A strategy deployment interface showing: "
            "a list of available trading strategy templates, "
            "market category selection (Politics, Crypto, Sports, Events), "
            "risk level selector, and deploy button. Simple card UI."
        ),
        "srcset_sizes": ["339x265"],
    },
    {
        "filename": "risk_management.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A risk management configuration panel showing: "
            "position sizing rules editor with sliders, "
            "maximum loss limits input fields, "
            "portfolio exposure controls, "
            "API integration settings panel, "
            "and risk metrics dashboard. Professional settings UI layout."
        ),
        "srcset_sizes": ["1335x1282"],
    },
    {
        "filename": "strategy_config.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A strategy builder and configuration screen showing: "
            "strategy template selection dropdown, "
            "parameter customization sliders and inputs, "
            "portfolio allocation pie chart, "
            "risk parameter adjustment controls, "
            "and a preview of strategy logic flow."
        ),
        "srcset_sizes": ["1323x1085"],
    },
    {
        "filename": "performance_metrics.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A strategy performance analytics dashboard showing: "
            "historical profit/loss line chart, "
            "win rate percentage gauge (72%), "
            "return metrics table (ROI, Sharpe ratio, max drawdown), "
            "backtesting results comparison bars, "
            "and monthly performance heatmap."
        ),
        "srcset_sizes": ["1382x753"],
    },
    {
        "filename": "staking_banner.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A wide horizontal banner showing staking rewards timeline: "
            "a horizontal timeline with milestones, "
            "staking reward tiers (Bronze/Silver/Gold), "
            "APY percentages, compounding earnings graph, "
            "and total staked amount display. Very wide landscape format."
        ),
        "srcset_sizes": ["1561x277"],
    },
    {
        "filename": "staking_results.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A staking results and rewards summary interface showing: "
            "total rewards earned display, "
            "staking period timeline, "
            "reward claim button, "
            "breakdown of rewards by strategy, "
            "and compound interest calculator."
        ),
        "srcset_sizes": ["1503x858"],
    },
    {
        "filename": "auto_strategy.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "An automated trading strategy execution flow diagram showing: "
            "market scanning process arrows, "
            "signal detection algorithm boxes, "
            "automated order placement workflow, "
            "profit extraction and reinvestment cycle, "
            "and real-time execution logs."
        ),
        "srcset_sizes": ["1131x606"],
    },
    {
        "filename": "arbitrum_governance.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "An Arbitrum DAO governance interface integrated with PolyBot showing: "
            "Arbitrum logo (triangle/arrow symbol) prominently, "
            "active governance proposals list, "
            "voting power display, "
            "PolyBot trading metrics on Arbitrum network, "
            "and cross-chain bridge status."
        ),
        "srcset_sizes": ["1976x1176"],
    },
    {
        "filename": "election_mobile.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A mobile-optimized view of election prediction market trading showing: "
            "election market categories list, "
            "candidate odds display (Democrat vs Republican), "
            "PolyBot automated position tracker, "
            "47% return achievement badge, "
            "and position management controls. Tall portrait format."
        ),
        "srcset_sizes": ["668x1176"],
    },
    {
        "filename": "zksync_governance.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A zkSync ecosystem interface showing election market alpha trading: "
            "zkSync logo (lightning bolt) prominently, "
            "election prediction market positions, "
            "automated position management diagram, "
            "47% returns achievement display, "
            "and zkSync transaction speed metrics."
        ),
        "srcset_sizes": ["900x1024"],
    },
    {
        "filename": "crypto_sweep.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A cryptocurrency market sweep trading results dashboard showing: "
            "Bitcoin ETF approval event timeline, "
            "PolyBot prediction accuracy chart, "
            "total profits from crypto prediction markets, "
            "precision timing indicators with arrows, "
            "and before/after market position comparison."
        ),
        "srcset_sizes": ["896x660"],
    },
    {
        "filename": "uniswap_performance.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A Uniswap DAO performance dashboard showing Q4 2025 results: "
            "Uniswap unicorn logo prominently, "
            "Q4 2025 performance chart with gains highlighted, "
            "Bitcoin ETF trading success metrics, "
            "PolyBot strategy comparison table, "
            "and governance token staking rewards."
        ),
        "srcset_sizes": ["1150x802"],
    },
    {
        "filename": "wormhole_sports.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A Wormhole cross-chain sports prediction market interface showing: "
            "Wormhole bridge/portal logo, "
            "sports market categories (NFL, NBA, Soccer, Tennis), "
            "odds analysis algorithm output, "
            "value bet identification markers, "
            "and expected return calculations."
        ),
        "srcset_sizes": ["768x964"],
    },
    {
        "filename": "obol_multicategory.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "An Obol multi-category prediction market strategy dashboard showing: "
            "Obol network logo, "
            "four market category tiles (Politics, Sports, Crypto, World Events), "
            "diversification pie chart across categories, "
            "real-time risk management gauges, "
            "and automated strategy allocation controls."
        ),
        "srcset_sizes": ["633x694"],
    },
    {
        "filename": "logos_horizontal.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A horizontal strip of DeFi/blockchain protocol logos drawn in sketch style: "
            "zkSync (lightning bolt), EigenLayer (E logo), Polymarket (P logo), "
            "Compound (green leaf), Aave (ghost), evenly spaced in a row. "
            "Wide landscape format, minimal and clean."
        ),
        "srcset_sizes": ["617x128"],
    },
    {
        "filename": "logos_hires.png",
        "prompt": EXCALIDRAW_PREFIX + (
            "A wide horizontal arrangement of DeFi protocol partner logos in sketch style: "
            "Compound (leaf icon + text), Obol (circle logo), Hyperlane (chain links), "
            "Lido (steth), Curve (wave), Aave (ghost), evenly distributed. "
            "Extra wide format, centered logos with names below each."
        ),
        "srcset_sizes": ["2500x1000"],
    },
    {
        "filename": "icon_check.png",
        "prompt": (
            "A single small excalidraw-style icon: a simple hand-drawn checkmark "
            "or bullet point icon in black on white background. "
            "Very minimal, just the icon centered on white. No text."
        ),
        "srcset_sizes": ["24x24"],
    },
    {
        "filename": "arrow_left.png",
        "prompt": (
            "A single small excalidraw-style icon: a left-pointing arrow "
            "inside a circle, hand-drawn black lines on white background. "
            "Simple carousel navigation arrow button icon."
        ),
        "srcset_sizes": ["40x40"],
    },
    {
        "filename": "arrow_right.png",
        "prompt": (
            "A single small excalidraw-style icon: a right-pointing arrow "
            "inside a circle, hand-drawn black lines on white background. "
            "Simple carousel navigation arrow button icon."
        ),
        "srcset_sizes": ["40x40"],
    },
]


def generate_image(prompt, filename, max_retries=3):
    """Generate an image using Nano Banana Pro API with fallback."""
    output_path = f"/Users/bluequbit/dev/polymer/site/images/{filename}"

    if os.path.exists(output_path):
        print(f"  [SKIP] {filename} already exists")
        return True

    for model in MODELS:
        for attempt in range(max_retries):
            try:
                print(f"  Generating {filename} with {model} (attempt {attempt+1})...")
                response = client.models.generate_content(
                    model=model,
                    contents=prompt,
                    config=types.GenerateContentConfig(
                        response_modalities=['TEXT', 'IMAGE']
                    )
                )

                for part in response.candidates[0].content.parts:
                    if hasattr(part, 'inline_data') and part.inline_data and part.inline_data.data:
                        with open(output_path, 'wb') as f:
                            f.write(part.inline_data.data)
                        size_kb = len(part.inline_data.data) // 1024
                        print(f"  [OK] {filename} saved ({size_kb}KB)")
                        return True

                print(f"  No image data in response for {filename}")

            except Exception as e:
                err = str(e)
                if '503' in err or 'UNAVAILABLE' in err:
                    print(f"  Model {model} unavailable, waiting 3s...")
                    time.sleep(3)
                elif '429' in err or 'RATE' in err:
                    print(f"  Rate limited, waiting 10s...")
                    time.sleep(10)
                else:
                    print(f"  Error with {model}: {err[:120]}")
                    break  # Try next model

    print(f"  [FAIL] Could not generate {filename}")
    return False


def main():
    print(f"Generating {len(IMAGES)} images for PolyBot site...")
    print("=" * 60)

    success = 0
    failed = []

    for i, img in enumerate(IMAGES, 1):
        print(f"\n[{i}/{len(IMAGES)}] {img['filename']}")
        ok = generate_image(img['prompt'], img['filename'])
        if ok:
            success += 1
        else:
            failed.append(img['filename'])
        # Small delay to avoid rate limiting
        time.sleep(1)

    print("\n" + "=" * 60)
    print(f"Done! {success}/{len(IMAGES)} images generated successfully.")
    if failed:
        print(f"Failed: {failed}")


if __name__ == '__main__':
    main()
