import { beforeEach, describe, expect, it } from "vitest";
import { filterWith, partition, setKey } from "lamb";

import {
  cacheHistory,
  cacheSpentNotes,
  cacheUnspentNotes,
} from "$lib/mock-data";
import { arrayMaxByKey } from "$lib/dusk/array";
import {
  fillCacheDatabase,
  getCacheDatabase,
  getCacheTableCount,
  sortCacheNotes,
} from "$lib/test-helpers";

import walletCache from "..";

const getMaxLastBlockHeight = arrayMaxByKey("lastBlockHeight");
const getMaxPos = arrayMaxByKey("pos");

describe("Wallet cache", () => {
  const db = getCacheDatabase();

  beforeEach(async () => {
    await getCacheDatabase().delete();
    await fillCacheDatabase();
  });

  describe("Reading and clearing the cache", async () => {
    const psk = cacheHistory[0].psk;

    /** @type {(entries: Array<{ psk: string }>) => typeof entries} */
    const filterByPsk = filterWith((entry) => entry.psk === psk);

    it("should expose a method to clear the database", async () => {
      await db.open();
      await expect(db.table("history").count()).resolves.toBe(
        cacheHistory.length
      );
      await expect(db.table("spentNotes").count()).resolves.toBe(
        cacheSpentNotes.length
      );
      await expect(db.table("unspentNotes").count()).resolves.toBe(
        cacheUnspentNotes.length
      );

      /**
       * Closing the db to suppress the warning about another
       * connection wanting to delete the database.
       */
      db.close();

      await walletCache.clear();

      await db.open();

      await expect(db.table("history").count()).resolves.toBe(0);
      await expect(db.table("spentNotes").count()).resolves.toBe(0);
      await expect(db.table("unspentNotes").count()).resolves.toBe(0);

      db.close();
    });

    it("should expose a method to retrieve all existing notes and optionally filter them by a `psk`", async () => {
      const allDbNotes = sortCacheNotes(await walletCache.getAllNotes());
      const allNotes = sortCacheNotes(
        cacheUnspentNotes.concat(cacheSpentNotes)
      );
      const dbNotesByPsk = sortCacheNotes(await walletCache.getAllNotes(psk));
      const notesByPsk = filterByPsk(allNotes);

      expect(allDbNotes).toStrictEqual(allNotes);
      expect(dbNotesByPsk).toStrictEqual(notesByPsk);
      await expect(walletCache.getAllNotes("foo")).resolves.toStrictEqual([]);
    });

    it("should expose a method to retrieve the cached history for a specific `psk`", async () => {
      const history = filterByPsk(cacheHistory)[0];

      await expect(walletCache.getHistoryEntry(psk)).resolves.toStrictEqual(
        history
      );
      await expect(walletCache.getHistoryEntry("foo")).resolves.toBeUndefined();
    });

    it("should expose a method to retrieve the spent notes and optionally filter them by a `psk`", async () => {
      const spentDbNotes = sortCacheNotes(await walletCache.getSpentNotes());
      const spentNotes = sortCacheNotes(cacheSpentNotes);
      const spentDbNotesByPsk = sortCacheNotes(
        await walletCache.getSpentNotes(psk)
      );
      const spentNotesByPsk = filterByPsk(spentNotes);

      expect(spentDbNotes).toStrictEqual(spentNotes);
      expect(spentDbNotesByPsk).toStrictEqual(spentNotesByPsk);
      await expect(walletCache.getSpentNotes("foo")).resolves.toStrictEqual([]);
    });

    it("should expose a method to retrieve the unspent notes and optionally filter them by a `psk`", async () => {
      const unspentDbNotes = sortCacheNotes(
        await walletCache.getUnspentNotes()
      );
      const unspentNotes = sortCacheNotes(cacheUnspentNotes);
      const unspentDbNotesByPsk = sortCacheNotes(
        await walletCache.getUnspentNotes(psk)
      );
      const unspentNotesByPsk = filterByPsk(unspentNotes);

      expect(unspentDbNotes).toStrictEqual(unspentNotes);
      expect(unspentDbNotesByPsk).toStrictEqual(unspentNotesByPsk);
      await expect(walletCache.getUnspentNotes("foo")).resolves.toStrictEqual(
        []
      );
    });
  });

  describe("Writing the history", () => {
    const newBlockHeight = getMaxLastBlockHeight(cacheHistory) + 1;

    /* eslint-disable camelcase */

    /** @type {Transaction} */
    const tx = {
      amount: 25000,
      block_height: newBlockHeight,
      direction: "In",
      fee: 1876,
      id: "some-tx-id",
      tx_type: "TRANSFER",
    };

    /* eslint-enable camelcase */

    const newEntry = {
      history: [tx],
      lastBlockHeight: newBlockHeight,
      psk: "some-new-psk",
    };

    it("should expose a method to set a new cache history entry", async () => {
      await walletCache.setHistoryEntry(newEntry);

      await expect(
        walletCache.getHistoryEntry(newEntry.psk)
      ).resolves.toStrictEqual(newEntry);

      // the other entries are still there
      for (const entry of cacheHistory) {
        await expect(
          walletCache.getHistoryEntry(entry.psk)
        ).resolves.toStrictEqual(entry);
      }

      await expect(getCacheTableCount("history")).resolves.toBe(
        cacheHistory.length + 1
      );
    });

    it("should replace the old entry, removing duplicate transactions, if the `psk` already exists", async () => {
      const psk = cacheHistory[0].psk;
      const updatedEntry = {
        ...newEntry,
        history: [...cacheHistory[0].history, ...newEntry.history],
        psk,
      };

      await walletCache.setHistoryEntry(updatedEntry);

      await expect(walletCache.getHistoryEntry(psk)).resolves.toStrictEqual(
        updatedEntry
      );

      // the other entry is still there
      await expect(
        walletCache.getHistoryEntry(cacheHistory[1].psk)
      ).resolves.toStrictEqual(cacheHistory[1]);
      await expect(getCacheTableCount("history")).resolves.toBe(
        cacheHistory.length
      );
    });

    it("should leave the history as it was before if an error occurs during writing", async () => {
      await expect(
        // @ts-expect-error
        walletCache.setHistoryEntry({ history: [] })
      ).rejects.toBeInstanceOf(Error);

      for (const entry of cacheHistory) {
        await expect(
          walletCache.getHistoryEntry(entry.psk)
        ).resolves.toStrictEqual(entry);
      }

      await expect(getCacheTableCount("history")).resolves.toBe(
        cacheHistory.length
      );
    });
  });

  describe("Adding notes", () => {
    const psk = cacheUnspentNotes[0].psk;

    /** @type {(note: WalletCacheNote) => boolean} */
    const hasTestPsk = (note) => note.psk === psk;

    /* eslint-disable camelcase */

    /** @type {WalletCacheNote} */
    const newNote = {
      block_height: getMaxLastBlockHeight(cacheHistory) + 1,
      note: [],
      nullifier: Array(32).fill(0),
      pos: getMaxPos(cacheSpentNotes.concat(cacheUnspentNotes)) + 1,
      psk,
    };

    /* eslint-enable camelcase */

    it("should expose a method to add new notes to the spent list, which also deletes unspent notes that are now spent", async () => {
      const [notesBeingSpent, expectedUnspentNotes] = partition(
        cacheUnspentNotes,
        hasTestPsk
      );
      const spentNoteDuplicate = cacheSpentNotes.find(hasTestPsk);

      if (!spentNoteDuplicate) {
        throw new Error(
          "No suitable spent note found to setup the duplicate test"
        );
      }

      /*
       * We add some notes from the unspent list to verify that they
       * change their state to spent and an existing spent note to
       * verify that duplicates aren't being added.
       */
      const newNotes = notesBeingSpent.concat(newNote, spentNoteDuplicate);
      const expectedSpentNotes = cacheSpentNotes.concat(
        newNote,
        notesBeingSpent
      );

      await walletCache.addSpentNotes(newNotes);

      await expect(
        walletCache.getUnspentNotes().then(sortCacheNotes)
      ).resolves.toStrictEqual(sortCacheNotes(expectedUnspentNotes));
      await expect(
        walletCache.getSpentNotes().then(sortCacheNotes)
      ).resolves.toStrictEqual(sortCacheNotes(expectedSpentNotes));
      await expect(getCacheTableCount("spentNotes")).resolves.toBe(
        cacheSpentNotes.length + notesBeingSpent.length + 1
      );
      await expect(getCacheTableCount("unspentNotes")).resolves.toBe(
        cacheUnspentNotes.length - notesBeingSpent.length
      );
    });

    it("should leave both the spent and unspent notes as they were if an error occurs during insertion", async () => {
      // @ts-expect-error
      const newNotes = cacheUnspentNotes.concat({});

      await expect(walletCache.addSpentNotes(newNotes)).rejects.toBeInstanceOf(
        Error
      );

      const allNotes = sortCacheNotes(await walletCache.getAllNotes());

      expect(
        sortCacheNotes(cacheSpentNotes.concat(cacheUnspentNotes))
      ).toStrictEqual(allNotes);
    });

    it("should expose a method to add new notes to the unspent list", async () => {
      /*
       * We just pick some notes to add from the spent list for the test,
       * as we just need to see that they are added.
       * Notes can't go from spent to unspent anyway.
       */
      const unspentNotesToAdd = cacheSpentNotes.map(setKey("psk", psk));

      const unspentNoteDuplicate = cacheUnspentNotes.find(hasTestPsk);

      if (!unspentNoteDuplicate) {
        throw new Error(
          "No suitable unspent note found to setup the duplicate test"
        );
      }

      /* As before we pick also a existing unspent note to verify
       * that duplicates aren't being added.
       */
      const newNotes = unspentNotesToAdd.concat(newNote, unspentNoteDuplicate);
      const expectedUnspentNotes = cacheUnspentNotes.concat(
        newNote,
        unspentNotesToAdd
      );

      await walletCache.addUnspentNotes(newNotes);

      await expect(
        walletCache.getUnspentNotes().then(sortCacheNotes)
      ).resolves.toStrictEqual(sortCacheNotes(expectedUnspentNotes));
    });

    it("should leave the unspent notes as they were if an error occurs during insertion", async () => {
      // @ts-expect-error
      const newNotes = cacheSpentNotes.concat({});

      await expect(walletCache.addSpentNotes(newNotes)).rejects.toBeInstanceOf(
        Error
      );

      expect(sortCacheNotes(cacheUnspentNotes)).toStrictEqual(
        sortCacheNotes(await walletCache.getUnspentNotes())
      );
    });
  });
});
