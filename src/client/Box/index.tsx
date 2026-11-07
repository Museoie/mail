import { Suspense, UIEventHandler, useContext, useState } from "react";
import { Writer, Accounts, Mails } from "./components";
import { Context } from "client";
import "./index.scss";

export * from "./components";

// The shell is a CSS grid (design doc §4): an 80px rail, the 320px accounts
// column, and the centered message list. The accounts column leaves the grid
// and becomes an overlay below 1050px; the rail becomes a bottom bar below
// 750px (see Box/index.scss). Compose docks at the right edge of the grid,
// fully on-screen — there are no negative pane offsets anywhere in this
// shell, and no swipe math: the rail buttons are the explicit controls.
const Box = () => {
  const {
    isAccountsOpen,
    setIsAccountsOpen,
    isWriterOpen,
    setIsWriterOpen
  } = useContext(Context);

  const [page, setPage] = useState(1);

  const onCenterScroll: UIEventHandler<HTMLDivElement> = (e) => {
    const { scrollHeight, offsetHeight, scrollTop } = e.currentTarget;
    if (scrollHeight - 100 <= offsetHeight + scrollTop) setPage(page + 1);
  };

  const onClickCenterCurtain = () => {
    if (!isWriterOpen) setIsAccountsOpen(false);
    setIsWriterOpen(false);
  };

  return (
    <div className={"shell" + (isAccountsOpen ? "" : " accounts-closed")}>
      <Suspense fallback={<></>}>
        <Accounts setPage={setPage} />
      </Suspense>
      <div
        className={
          "center-col" + (isAccountsOpen ? " lock-scroll" : "")
        }
        onScroll={onCenterScroll}
      >
        <div
          className={
            "curtain center-curtain" +
            (isWriterOpen || isAccountsOpen ? " on" : "") +
            (isWriterOpen ? " on-writer" : "") +
            (isAccountsOpen ? " on-accounts" : "")
          }
          onClick={onClickCenterCurtain}
        />
        <div className="center-inner">
          <Suspense fallback={<></>}>
            <Mails page={page} />
          </Suspense>
        </div>
      </div>
      <div className={"writer-pane" + (isWriterOpen ? " open" : "")}>
        <Suspense fallback={<></>}>
          <Writer />
        </Suspense>
      </div>
    </div>
  );
};

export default Box;
