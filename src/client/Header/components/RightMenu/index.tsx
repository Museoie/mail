import { useContext } from "react";
import { Context, onKeyboardActivate } from "client";
import SearchIcon from "client/Box/components/Accounts/components/SearchIcon";
import WriteIcon from "./components/WriteIcon";

const RightMenu = () => {
  const { isWriterOpen, setIsWriterOpen, enterSearch } = useContext(Context);

  const onClickWriter = () => {
    setIsWriterOpen(!isWriterOpen);
  };

  return (
    <div className="menu right">
      <div
        className="cursor"
        role="button"
        tabIndex={0}
        aria-label="Search"
        onClick={enterSearch}
        onKeyDown={onKeyboardActivate(enterSearch)}
      >
        <div className="iconBox">
          <SearchIcon />
        </div>
      </div>
      <div
        className="cursor"
        role="button"
        tabIndex={0}
        aria-label={isWriterOpen ? "Close compose" : "Compose new mail"}
        onClick={onClickWriter}
        onKeyDown={onKeyboardActivate(onClickWriter)}
      >
        <div id="write" className="iconBox">
          <WriteIcon />
        </div>
      </div>
    </div>
  );
};

export default RightMenu;
