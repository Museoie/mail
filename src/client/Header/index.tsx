import { useContext } from "react";
import { Context } from "client";
import LeftMenu from "./components/LeftMenu";
import RightMenu from "./components/RightMenu";
import "./index.scss";

const Header = () => {
  const { viewSize, domainName, userInfo, selectedAccount, newMailsTotal } =
    useContext(Context);

  const username = userInfo?.username;
  const fullEmailHostName =
    username === "admin"
      ? domainName
      : [username, domainName].filter((e) => e).join(".");

  const title = !selectedAccount
    ? "@" + fullEmailHostName
    : viewSize.width > 750
    ? selectedAccount
    : selectedAccount.split("@")[0];

  return (
    <div id="title_bar" className="sticky">
      {userInfo ? <LeftMenu /> : null}
      <h1>{title}</h1>
      {userInfo && newMailsTotal > 0 ? (
        <span
          className="header-unread-count"
          title={`${newMailsTotal} unread`}
        >
          {newMailsTotal}
        </span>
      ) : null}
      {userInfo ? <RightMenu /> : null}
    </div>
  );
};

export default Header;
