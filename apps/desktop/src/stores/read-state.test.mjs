import { describe, it, expect, mock } from 'bun:test';
import * as bridge from '../lib/bridge';
let accountRead;
const pendingSnapshots = new Map();
const account = {
  contract_version: 1, network: 'testnet', address: '0x0000000000000000000000000000000000000001',
  as_of_ms: 1000, feed_age_ms: 0, feed: 'live',
  balances: { equity_usd: '125.00', total_margin_used_usd: '5.00', perps_account_value_usd: '125.00', spot_usdc_available: '0', withdrawable_usd: '120' },
  positions: [], orders: [],
};
mock.module('../lib/bridge', () => ({
  ...bridge, inTauri: () => true, fetchAccountState: () => accountRead(), watchMarket: async () => {},
  fetchMarketSnapshot: (_network, symbol) => new Promise(resolve => pendingSnapshots.set(symbol, resolve)),
  fetchChartSeries: async (_network, symbol, interval) => ({ symbol, interval, interval_ms: 3600000, price_decimals: 2, closed: [] }),
}));
const { shell, refreshAccount } = await import('./shell');
const { market, select, refreshSnapshot, applyFeed } = await import('./market');
const snapshot = (symbol, at) => ({ symbol, as_of_ms: at, bids: [], asks: [], book: { depth: [] }, funding: {}, vol: {} });
async function untilSnapshot(symbol) {
  for (let i = 0; i < 20 && !pendingSnapshots.has(symbol); i++) await Promise.resolve();
  expect(pendingSnapshots.has(symbol)).toBe(true);
}
describe('account read states', () => {
  it('distinguishes unread from empty and retains the last good account on failure', async () => {
    expect(shell.account).toBe(null);
    accountRead = async () => { throw { kind: 'not_configured', detail: 'Configure an account.' }; };
    await refreshAccount();
    expect(shell.account).toBe(null);
    expect(shell.accountError).toBe('Configure an account.');
    expect(shell.feeds.rest).toBe('unknown');
    accountRead = async () => account;
    await refreshAccount();
    expect(shell.account.positions).toEqual([]);
    expect(shell.account.balances.equity_usd).toBe('125.00');
    expect(shell.accountError).toBe(null);
    accountRead = async () => { throw { kind: 'venue', detail: 'Read timed out.' }; };
    await refreshAccount();
    expect(shell.account.as_of_ms).toBe(1000);
    expect(shell.account.balances.equity_usd).toBe('125.00');
    expect(shell.accountError).toBe('Read timed out.');
    expect(shell.feeds.rest).toBe('down');
  });
});
describe('market selection and derived freshness', () => {
  it('does not land an old snapshot under a new symbol', async () => {
    const btc = select('BTC'); await untilSnapshot('BTC');
    const eth = select('ETH'); await untilSnapshot('ETH');
    pendingSnapshots.get('ETH')(snapshot('ETH', 2000)); await eth;
    pendingSnapshots.get('BTC')(snapshot('BTC', 1000)); await btc;
    expect(market.selected).toBe('ETH');
    expect(market.snapshot.symbol).toBe('ETH');
    expect(market.featuresReadMs).toBe(2000);
  });
  it('book ticks do not freshen derived packs and refresh preserves the chart', async () => {
    applyFeed({ kind: 'book', coin: 'ETH', at_ms: 3000, bids: [], asks: [] });
    expect(market.snapshot.as_of_ms).toBe(3000);
    expect(market.featuresReadMs).toBe(2000);
    const chart = market.chart;
    pendingSnapshots.delete('ETH');
    const reading = refreshSnapshot(); await untilSnapshot('ETH');
    expect(market.chart).toBe(chart);
    expect(market.snapshot.as_of_ms).toBe(3000);
    pendingSnapshots.get('ETH')(snapshot('ETH', 4000)); await reading;
    expect(market.chart).toBe(chart);
    expect(market.featuresReadMs).toBe(4000);
  });
});
