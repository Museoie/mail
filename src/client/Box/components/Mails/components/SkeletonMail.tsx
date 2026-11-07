import { useContext } from "react";
import { Context } from "client";

// Loading rows mirror the collapsed row lines (design doc §5): sender,
// subject, snippet in Comfortable; sender and subject only in Compact, where
// the snippet is hidden.
const SkeletonMails = () => {
  const { density } = useContext(Context);
  const rand = Math.random;

  const bars = [
    <div key="sender" style={{ width: 30 + 20 * rand() + "%" }} />,
    <div key="subject" style={{ width: 60 + 20 * rand() + "%" }} />
  ];
  if (density === "comfortable") {
    bars.push(
      <div key="snippet" style={{ width: 80 + 15 * rand() + "%" }} />
    );
  }

  return (
    <>
      <blockquote className="mailcard skeleton loading_animation">
        {bars}
      </blockquote>
    </>
  );
};

export default SkeletonMails;
