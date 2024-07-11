import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/svelte";
import { get } from "svelte/store";

import { duskAPI } from "$lib/services";
import { transformTransaction } from "$lib/chain-info";
import { appStore } from "$lib/stores";
import { gqlTransactions } from "$lib/mock-data";

import Transactions from "../+page.svelte";

describe("Transactions page", () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(2024, 4, 30));

  const { fetchInterval, network, transactionsListEntries } = get(appStore);
  const getTransactionSpy = vi
    .spyOn(duskAPI, "getTransactions")
    .mockResolvedValue(gqlTransactions.transactions.map(transformTransaction));

  afterEach(() => {
    cleanup();
    getTransactionSpy.mockClear();
  });

  afterAll(() => {
    vi.useRealTimers();
    getTransactionSpy.mockRestore();
  });

  it("should render the Transactions page, start polling for blocks and stop the polling when unmounted", async () => {
    const { container, unmount } = render(Transactions);

    // snapshost in loading state
    expect(container.firstChild).toMatchSnapshot();
    expect(getTransactionSpy).toHaveBeenCalledTimes(1);
    expect(getTransactionSpy).toHaveBeenNthCalledWith(
      1,
      network,
      transactionsListEntries
    );

    await vi.advanceTimersByTimeAsync(1);

    // snapshot with received data from GraphQL
    expect(container.firstChild).toMatchSnapshot();

    await vi.advanceTimersByTimeAsync(fetchInterval - 1);

    expect(getTransactionSpy).toHaveBeenCalledTimes(2);
    expect(getTransactionSpy).toHaveBeenNthCalledWith(
      2,
      network,
      transactionsListEntries
    );

    await vi.advanceTimersByTimeAsync(fetchInterval);

    expect(getTransactionSpy).toHaveBeenCalledTimes(3);
    expect(getTransactionSpy).toHaveBeenNthCalledWith(
      3,
      network,
      transactionsListEntries
    );

    unmount();

    await vi.advanceTimersByTimeAsync(fetchInterval * 10);

    expect(getTransactionSpy).toHaveBeenCalledTimes(3);
  });
});
